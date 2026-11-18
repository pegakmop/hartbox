// ANSI/VT100 terminal emulation for the guest console.
//
// The guest sees an 80x24 serial console, so the screen model is exactly that:
// 24 rows with a scrollback ring behind it. Only the subset of escape
// sequences a Linux serial console and BusyBox shell actually emit is
// implemented, plus a cursor-position report because the shell asks for one
// while starting up.

import Foundation
import UIKit

/// One character cell.
struct Cell: Equatable {
    var ch: Character = " "
    var fg: UInt8 = 7
    var bold: Bool = false

    static let blank = Cell()
}

final class Terminal {
    let cols: Int
    let rows: Int
    private static let scrollbackLimit = 2000

    /// The visible 80x24 screen.
    private(set) var screen: [[Cell]]
    /// Complete lines that scrolled off the top, oldest first.
    private(set) var scrollback: [[Cell]] = []

    /// Cursor position on the visible screen.
    private(set) var row = 0
    private(set) var col = 0
    private(set) var cursorVisible = true

    /// Set whenever the screen changed, cleared by the view after a redraw.
    var dirty = true

    /// Bytes the terminal itself must send back to the guest (e.g. a cursor
    /// position report). The app forwards these to the emulator.
    var responder: ((Data) -> Void)?

    private var fg: UInt8 = 7
    private var bold = false
    private var wrapPending = false
    private var savedRow = 0
    private var savedCol = 0

    private enum Parser {
        case ground
        case escape
        case csi
        case osc
        case charset
    }
    private var parser: Parser = .ground
    private var params: [Int] = []
    private var param: Int = 0
    private var hasParam = false
    private var csiPrivate = false

    init(cols: Int = 80, rows: Int = 24) {
        self.cols = cols
        self.rows = rows
        screen = Array(repeating: Array(repeating: Cell.blank, count: cols), count: rows)
    }

    // MARK: - Feeding

    func feed(_ data: Data) {
        for byte in data {
            step(byte)
        }
        dirty = true
    }

    private func step(_ byte: UInt8) {
        switch parser {
        case .ground: ground(byte)
        case .escape: escape(byte)
        case .csi: csi(byte)
        case .osc:
            if byte == 0x07 || byte == 0x1B { parser = byte == 0x1B ? .escape : .ground }
        case .charset: parser = .ground
        }
    }

    private func ground(_ b: UInt8) {
        switch b {
        case 0x1B: parser = .escape
        case 0x0A, 0x0B, 0x0C: lineFeed()
        case 0x0D: col = 0; wrapPending = false
        case 0x08: col = max(0, col - 1); wrapPending = false
        case 0x09: col = min(cols - 1, (col / 8 + 1) * 8); wrapPending = false
        case 0x07: break
        case 0x00...0x1F: break
        case 0x20...0x7E:
            put(Character(UnicodeScalar(b)))
        default:
            // Treat any byte of a UTF-8 sequence as its own scalar; the guest
            // console is ASCII in practice.
            if let scalar = UnicodeScalar(UInt32(b)) { put(Character(scalar)) }
        }
    }

    private func escape(_ b: UInt8) {
        parser = .ground
        switch b {
        case 0x5B: // '['
            params = []; param = 0; hasParam = false; csiPrivate = false
            parser = .csi
        case 0x5D: parser = .osc // ']'
        case 0x28, 0x29, 0x2A, 0x2B: parser = .charset
        case 0x37: savedRow = row; savedCol = col // '7'
        case 0x38: row = savedRow; col = savedCol; wrapPending = false // '8'
        case 0x63: reset() // 'c'
        default: break
        }
    }

    private func csi(_ b: UInt8) {
        switch b {
        case 0x30...0x39: // 0-9
            param = param * 10 + Int(b - 0x30)
            hasParam = true
        case 0x3B: // ';'
            params.append(hasParam ? param : 0)
            param = 0
            hasParam = false
        case 0x3F: csiPrivate = true // '?'
        case 0x20...0x2F: break // intermediates
        default:
            if hasParam { params.append(param) }
            param = 0
            hasParam = false
            parser = .ground
            execute(Character(UnicodeScalar(b)), privateMode: csiPrivate)
        }
    }

    private func arg(_ i: Int, _ fallback: Int = 1) -> Int {
        i < params.count ? params[i] : fallback
    }

    /// Movement parameters default to 1, and an explicit 0 also means 1.
    private func move(_ i: Int) -> Int {
        max(1, arg(i, 1))
    }

    private func execute(_ cmd: Character, privateMode: Bool) {
        switch cmd {
        case "A": row = max(0, row - move(0))
        case "B": row = min(rows - 1, row + move(0))
        case "C": col = min(cols - 1, col + move(0))
        case "D": col = max(0, col - move(0))
        case "E": row = min(rows - 1, row + move(0)); col = 0
        case "F": row = max(0, row - move(0)); col = 0
        case "G": col = min(cols - 1, max(0, move(0) - 1))
        case "d": row = min(rows - 1, max(0, move(0) - 1))
        case "H", "f":
            row = min(rows - 1, max(0, move(0) - 1))
            col = min(cols - 1, max(0, move(1) - 1))
        case "J": eraseDisplay(arg(0, 0))
        case "K": eraseLine(arg(0, 0))
        case "m": sgr()
        case "h", "l":
            if privateMode, arg(0) == 25 { cursorVisible = (cmd == "h") }
        case "n":
            if arg(0, 0) == 6 {
                let reply = "\u{1B}[\(row + 1);\(col + 1)R"
                responder?(Data(reply.utf8))
            }
        case "S": for _ in 0..<move(0) { scrollUp() }
        case "T": for _ in 0..<move(0) { scrollDown() }
        case "L": insertLines(move(0))
        case "M": deleteLines(move(0))
        case "P": deleteChars(move(0))
        case "@": insertChars(move(0))
        case "X": eraseChars(move(0))
        default: break
        }
        wrapPending = false
    }

    private func sgr() {
        let list = params.isEmpty ? [0] : params
        for p in list {
            switch p {
            case 0: fg = 7; bold = false
            case 1: bold = true
            case 22: bold = false
            case 30...37: fg = UInt8(p - 30)
            case 39: fg = 7
            case 90...97: fg = UInt8(p - 90); bold = true
            default: break
            }
        }
    }

    // MARK: - Screen operations

    private func put(_ ch: Character) {
        if wrapPending {
            col = 0
            lineFeed()
            wrapPending = false
        }
        screen[row][col] = Cell(ch: ch, fg: fg, bold: bold)
        if col == cols - 1 {
            wrapPending = true
        } else {
            col += 1
        }
    }

    private func lineFeed() {
        wrapPending = false
        if row == rows - 1 {
            scrollUp()
        } else {
            row += 1
        }
    }

    private func scrollUp() {
        scrollback.append(screen[0])
        if scrollback.count > Terminal.scrollbackLimit { scrollback.removeFirst() }
        screen.removeFirst()
        screen.append(Array(repeating: Cell.blank, count: cols))
    }

    private func scrollDown() {
        screen.removeLast()
        screen.insert(Array(repeating: Cell.blank, count: cols), at: 0)
    }

    private func insertLines(_ n: Int) {
        for _ in 0..<n {
            screen.removeLast()
            screen.insert(Array(repeating: Cell.blank, count: cols), at: row)
        }
    }

    private func deleteLines(_ n: Int) {
        for _ in 0..<n {
            screen.remove(at: row)
            screen.append(Array(repeating: Cell.blank, count: cols))
        }
    }

    private func deleteChars(_ n: Int) {
        var line = screen[row]
        let end = min(cols, col + n)
        if col < end {
            line.removeSubrange(col..<end)
            line.append(contentsOf: Array(repeating: Cell.blank, count: end - col))
            screen[row] = line
        }
    }

    private func insertChars(_ n: Int) {
        var line = screen[row]
        let count = min(n, cols - col)
        line.insert(contentsOf: Array(repeating: Cell.blank, count: count), at: col)
        line.removeLast(count)
        screen[row] = line
    }

    private func eraseChars(_ n: Int) {
        for c in col..<min(cols, col + n) { screen[row][c] = .blank }
    }

    private func eraseLine(_ mode: Int) {
        switch mode {
        case 0: for c in col..<cols { screen[row][c] = .blank }
        case 1: for c in 0...col { screen[row][c] = .blank }
        default: screen[row] = Array(repeating: Cell.blank, count: cols)
        }
    }

    private func eraseDisplay(_ mode: Int) {
        switch mode {
        case 0:
            eraseLine(0)
            for r in (row + 1)..<rows {
                screen[r] = Array(repeating: Cell.blank, count: cols)
            }
        case 1:
            eraseLine(1)
            for r in 0..<row {
                screen[r] = Array(repeating: Cell.blank, count: cols)
            }
        default:
            screen = Array(repeating: Array(repeating: Cell.blank, count: cols),
                           count: rows)
        }
    }

    private func reset() {
        screen = Array(repeating: Array(repeating: Cell.blank, count: cols),
                       count: rows)
        scrollback.removeAll()
        row = 0; col = 0; fg = 7; bold = false
        wrapPending = false
        cursorVisible = true
    }

    // MARK: - Display

    /// The lines to draw for a view that fits `maxRows` rows, `scrollbackOffset`
    /// lines above the live screen. Returns whole lines, each `cols` cells.
    func linesForDisplay(maxRows: Int, scrollbackOffset: Int) -> [[Cell]] {
        let all = scrollback + screen
        let total = all.count
        let firstVisible = max(0, total - rows - scrollbackOffset)
        let count = min(maxRows, total - firstVisible)
        return Array(all[firstVisible..<(firstVisible + count)])
    }

    /// Index in `linesForDisplay`'s array of the live screen's first row.
    func liveScreenStart(maxRows: Int, scrollbackOffset: Int) -> Int {
        let total = scrollback.count + rows
        let firstVisible = max(0, total - rows - scrollbackOffset)
        return scrollback.count - firstVisible
    }
}
