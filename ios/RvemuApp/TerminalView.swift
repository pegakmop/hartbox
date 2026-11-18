// The terminal surface. Tap it to type: every keystroke streams straight to
// the guest, the way a serial terminal works. The keys a phone keyboard lacks
// live in a bar attached to the top of the keyboard.

import SwiftUI
import UIKit

/// Hidden field that owns the keyboard. It never keeps text, so the screen
/// only ever shows the guest's echo.
final class TerminalInputField: UITextField {
    var onBytes: ((Data) -> Void)?

    override var canBecomeFirstResponder: Bool { true }

    /// Backspace is handled here rather than in the delegate: the field is
    /// always empty, so the delegate would only ever see zero-length edits.
    override func deleteBackward() {
        onBytes?(Data([0x7F]))
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var handled = false
        for press in presses {
            guard let key = press.key else { continue }
            let control = key.modifierFlags.contains(.control)
            switch key.keyCode {
            case .keyboardUpArrow: onBytes?(Data("\u{1B}[A".utf8)); handled = true
            case .keyboardDownArrow: onBytes?(Data("\u{1B}[B".utf8)); handled = true
            case .keyboardRightArrow: onBytes?(Data("\u{1B}[C".utf8)); handled = true
            case .keyboardLeftArrow: onBytes?(Data("\u{1B}[D".utf8)); handled = true
            case .keyboardEscape: onBytes?(Data([0x1B])); handled = true
            case .keyboardTab: onBytes?(Data([0x09])); handled = true
            case .keyboardDeleteOrBackspace: onBytes?(Data([0x7F])); handled = true
            default:
                // Control chords: ^A is 0x01, ^C is 0x03, and so on.
                if control, let first = key.charactersIgnoringModifiers.uppercased().first,
                   let ascii = first.asciiValue, ascii > 63, ascii < 128 {
                    onBytes?(Data([ascii - 64])); handled = true
                }
            }
        }
        if !handled { super.pressesBegan(presses, with: event) }
    }
}

/// A key in the accessory bar, carrying the bytes it sends.
private final class KeyButton: UIButton {
    var bytes = Data()
}

final class TerminalUIView: UIView, UITextFieldDelegate {
    var terminal: Terminal? {
        didSet { setNeedsDisplay() }
    }
    var onInput: ((Data) -> Void)?

    private let field = TerminalInputField()
    private var link: CADisplayLink?
    private var scrollOffset = 0
    private var blink = true
    private var lastBlink = Date()

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = Palette.terminalBackground
        isOpaque = true
        contentMode = .redraw

        field.delegate = self
        field.onBytes = { [weak self] bytes in self?.send(bytes) }
        field.keyboardType = .asciiCapable
        field.autocorrectionType = .no
        field.autocapitalizationType = .none
        field.spellCheckingType = .no
        field.smartQuotesType = .no
        field.smartDashesType = .no
        field.returnKeyType = .default
        field.textColor = .clear
        field.tintColor = .clear
        field.backgroundColor = .clear
        field.frame = CGRect(x: 0, y: 0, width: 1, height: 1)
        field.inputAccessoryView = Self.makeKeyBar(target: self)
        addSubview(field)

        addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(tapped)))
        addGestureRecognizer(UIPanGestureRecognizer(target: self, action: #selector(panned(_:))))

        NotificationCenter.default.addObserver(self, selector: #selector(focus),
                                               name: .rvemuFocusKeyboard, object: nil)
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        link?.invalidate()
        link = nil
        guard window != nil else { return }
        let l = CADisplayLink(target: self, selector: #selector(tick))
        l.preferredFrameRateRange = CAFrameRateRange(minimum: 20, maximum: 60, preferred: 60)
        l.add(to: .main, forMode: .common)
        link = l
    }

    /// Tap to type, tap again to put the keyboard away.
    @objc private func tapped() {
        if field.isFirstResponder {
            field.resignFirstResponder()
        } else {
            focus()
        }
    }

    @objc func focus() {
        field.becomeFirstResponder()
    }

    /// The sticky ctrl key: armed by one tap, spent by the next keystroke.
    private weak var ctrlButton: UIButton?
    private var ctrlArmed = false {
        didSet { ctrlButton?.isSelected = ctrlArmed }
    }

    /// The control character for a key: ^A..^Z are 1...26, plus the
    /// punctuation row (^@ ^[ ^\ ^] ^^ ^_ ^?) and the digits terminals use for
    /// the same codes (^2 ... ^8). Anything else has no control form.
    private static func control(_ b: UInt8) -> UInt8? {
        switch b {
        case 0x61...0x7A: return b - 0x60
        case 0x41...0x5A: return b - 0x40
        case 0x40, 0x20, 0x32: return 0x00
        case 0x5B, 0x33: return 0x1B
        case 0x5C, 0x34: return 0x1C
        case 0x5D, 0x35: return 0x1D
        case 0x5E, 0x36: return 0x1E
        case 0x5F, 0x37: return 0x1F
        case 0x3F, 0x38: return 0x7F
        default: return nil
        }
    }

    /// Spends the armed ctrl on the first key of `bytes`. Arrows become the
    /// xterm ctrl-arrow sequences and backspace becomes ^H.
    private func applyCtrl(_ bytes: Data) -> Data {
        ctrlArmed = false
        guard let first = bytes.first else { return bytes }
        if bytes.count == 3, first == 0x1B, bytes[1] == 0x5B, (0x41...0x44).contains(bytes[2]) {
            return Data("\u{1B}[1;5".utf8) + Data([bytes[2]])
        }
        if first == 0x7F { return Data([0x08]) + bytes.dropFirst() }
        guard let mapped = Self.control(first) else { return bytes }
        return Data([mapped]) + bytes.dropFirst()
    }

    private func send(_ input: Data) {
        guard !input.isEmpty else { return }
        let bytes = ctrlArmed ? applyCtrl(input) : input
        scrollOffset = 0
        blink = true
        lastBlink = Date()
        onInput?(bytes)
        setNeedsDisplay()
    }

    @objc private func tick() {
        guard let terminal else { return }
        var redraw = false
        if terminal.dirty {
            terminal.dirty = false
            redraw = true
        }
        if Date().timeIntervalSince(lastBlink) > 0.5 {
            lastBlink = Date()
            blink.toggle()
            redraw = redraw || field.isFirstResponder
        }
        if redraw { setNeedsDisplay() }
    }

    @objc private func panned(_ gesture: UIPanGestureRecognizer) {
        guard let terminal else { return }
        let lineHeight = metrics().lineHeight
        let dy = gesture.translation(in: self).y
        let lines = Int(dy / lineHeight)
        guard lines != 0 else { return }
        scrollOffset = min(max(0, terminal.scrollback.count), max(0, scrollOffset + lines))
        gesture.setTranslation(CGPoint(x: 0, y: dy - CGFloat(lines) * lineHeight), in: self)
        setNeedsDisplay()
    }

    // MARK: - Keyboard streaming

    func textField(_ field: UITextField, shouldChangeCharactersIn range: NSRange,
                   replacementString string: String) -> Bool {
        if !string.isEmpty {
            send(string == "\n" ? Data([0x0D]) : Data(string.utf8))
        }
        return false
    }

    func textFieldShouldReturn(_ field: UITextField) -> Bool {
        send(Data([0x0D]))
        return false
    }

    // MARK: - Keyboard accessory bar

    /// A row of keys, sized to share the keyboard's width exactly.
    private static func makeKeyBar(target: TerminalUIView) -> UIView {
        let bar = UIStackView()
        bar.axis = .horizontal
        bar.distribution = .fillEqually
        bar.alignment = .fill
        bar.backgroundColor = Palette.terminalBackground
        bar.frame = CGRect(x: 0, y: 0, width: UIScreen.main.bounds.width, height: 42)
        bar.autoresizingMask = [.flexibleWidth]
        let ctrl = key("ctrl", Data(), target)
        ctrl.removeTarget(target, action: #selector(keyTapped(_:)), for: .touchUpInside)
        ctrl.addTarget(target, action: #selector(ctrlTapped), for: .touchUpInside)
        target.ctrlButton = ctrl
        bar.addArrangedSubview(ctrl)
        for (label, bytes) in [
            ("tab", Data([0x09])),
            ("esc", Data([0x1B])),
            ("↑", Data("\u{1B}[A".utf8)),
            ("↓", Data("\u{1B}[B".utf8)),
            ("←", Data("\u{1B}[D".utf8)),
            ("→", Data("\u{1B}[C".utf8)),
        ] {
            bar.addArrangedSubview(key(label, bytes, target))
        }
        let down = UIButton(type: .system)
        var config = UIButton.Configuration.plain()
        config.contentInsets = NSDirectionalEdgeInsets(top: 0, leading: 2, bottom: 0, trailing: 2)
        config.image = UIImage(systemName: "keyboard.chevron.compact.down",
                               withConfiguration: UIImage.SymbolConfiguration(pointSize: 15, weight: .medium))
        down.configuration = config
        down.tintColor = Palette.foreground
        down.addTarget(target, action: #selector(dismissKeyboard), for: .touchUpInside)
        bar.addArrangedSubview(down)
        return bar
    }

    private static func key(_ label: String, _ bytes: Data, _ target: TerminalUIView) -> UIButton {
        let button = KeyButton(type: .system)
        button.bytes = bytes
        var config = UIButton.Configuration.plain()
        config.contentInsets = NSDirectionalEdgeInsets(top: 0, leading: 2, bottom: 0, trailing: 2)
        // The highlight is a small pill inside the key, not the whole cell.
        config.background.cornerRadius = 7
        config.background.backgroundInsets = NSDirectionalEdgeInsets(top: 7, leading: 3, bottom: 7, trailing: 3)
        config.attributedTitle = AttributedString(label, attributes: AttributeContainer([
            .font: UIFont.monospacedSystemFont(ofSize: 13, weight: .regular),
            .foregroundColor: Palette.foreground,
        ]))
        button.configuration = config
        button.configurationUpdateHandler = { button in
            button.configuration?.background.backgroundColor =
                button.isSelected ? Palette.blue.withAlphaComponent(0.4)
                : button.isHighlighted ? Palette.highlighted : .clear
        }
        button.addTarget(target, action: #selector(keyTapped(_:)), for: .touchUpInside)
        return button
    }

    @objc private func keyTapped(_ sender: KeyButton) {
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        send(sender.bytes)
    }

    @objc private func ctrlTapped() {
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        ctrlArmed.toggle()
    }

    @objc private func dismissKeyboard() {
        ctrlArmed = false
        field.resignFirstResponder()
        setNeedsDisplay()
    }

    // MARK: - Metrics

    private func metrics() -> (font: UIFont, bold: UIFont, cellWidth: CGFloat, lineHeight: CGFloat) {
        let probe = UIFont.monospacedSystemFont(ofSize: 100, weight: .regular)
        let probeWidth = ("M" as NSString).size(withAttributes: [.font: probe]).width
        let maxSize: CGFloat = traitCollection.verticalSizeClass == .compact ? 18 : 15
        let width = bounds.width - 2 * Layout.terminalInset
        let fit = width / CGFloat(terminal?.cols ?? 64) / (probeWidth / 100)
        let size = min(maxSize, max(8.5, fit))
        let font = UIFont.monospacedSystemFont(ofSize: size, weight: .regular)
        let bold = UIFont.monospacedSystemFont(ofSize: size, weight: .semibold)
        let cell = ("M" as NSString).size(withAttributes: [.font: font]).width
        return (font, bold, cell, (font.lineHeight).rounded())
    }

    // MARK: - Drawing

    override func draw(_ rect: CGRect) {
        guard let terminal else { return }
        let m = metrics()
        let visibleRows = max(1, Int((bounds.height - 2 * Layout.terminalInset) / m.lineHeight))
        let lines = terminal.linesForDisplay(maxRows: visibleRows, scrollbackOffset: scrollOffset)
        let liveStart = terminal.liveScreenStart(maxRows: visibleRows, scrollbackOffset: scrollOffset)
        let cursorLine = liveStart + terminal.row
        let showCursor = terminal.cursorVisible && scrollOffset == 0 && (blink || !field.isFirstResponder)
        let cursorCol = showCursor ? terminal.col : -1

        var index = 0
        for line in lines {
            let y = Layout.terminalInset + CGFloat(index) * m.lineHeight
            let live = index >= liveStart && index < liveStart + terminal.rows
            draw(line, at: y, metrics: m, cursorCol: (live && index == cursorLine) ? cursorCol : -1)
            index += 1
        }
    }

    private func draw(_ line: [Cell], at y: CGFloat,
                      metrics m: (font: UIFont, bold: UIFont, cellWidth: CGFloat, lineHeight: CGFloat),
                      cursorCol: Int) {
        let inset = Layout.terminalInset
        var index = 0
        while index < line.count {
            let cell = line[index]
            if cell == .blank, !line[(index + 1)...].contains(where: { $0 != .blank }) { break }
            var end = index + 1
            while end < line.count, line[end].fg == cell.fg, line[end].bold == cell.bold { end += 1 }
            let text = String(line[index..<end].map(\.ch))
            let x = inset + CGFloat(index) * m.cellWidth
            var attributes: [NSAttributedString.Key: Any] = [
                .font: cell.bold ? m.bold : m.font,
                .foregroundColor: Palette.ansi[Int(cell.fg) & 15],
            ]

            if cursorCol < index || cursorCol >= end {
                (text as NSString).draw(at: CGPoint(x: x, y: y), withAttributes: attributes)
            } else {
                let split = cursorCol - index
                let before = String(text.prefix(split))
                let cursor = String(text.dropFirst(split).prefix(1))
                let after = String(text.dropFirst(split + 1))
                if !before.isEmpty {
                    (before as NSString).draw(at: CGPoint(x: x, y: y), withAttributes: attributes)
                }
                let rect = CGRect(x: x + CGFloat(split) * m.cellWidth, y: y,
                                  width: m.cellWidth, height: m.lineHeight)
                Palette.blue.setFill()
                UIRectFill(rect)
                attributes[.foregroundColor] = Palette.terminalBackground
                (cursor as NSString).draw(at: CGPoint(x: rect.minX, y: y), withAttributes: attributes)
                if !after.isEmpty {
                    attributes[.foregroundColor] = Palette.ansi[Int(cell.fg) & 15]
                    (after as NSString).draw(at: CGPoint(x: rect.maxX, y: y), withAttributes: attributes)
                }
            }
            index = end
        }
    }
}

extension Notification.Name {
    /// Posted when the guest is ready for input, so the keyboard comes up.
    static let rvemuFocusKeyboard = Notification.Name("rvemuFocusKeyboard")
}

enum Layout {
    static let terminalInset: CGFloat = 12
    static let hairline: CGFloat = 1
}

struct TerminalPane: UIViewRepresentable {
    let terminal: Terminal
    var onInput: (Data) -> Void

    func makeUIView(context: Context) -> TerminalUIView {
        let view = TerminalUIView()
        view.terminal = terminal
        view.onInput = onInput
        return view
    }

    func updateUIView(_ uiView: TerminalUIView, context: Context) {
        if uiView.terminal !== terminal { uiView.terminal = terminal }
        uiView.onInput = onInput
    }
}
