// One screen: a status line and the terminal. Typing streams straight to the
// guest; the extra keys live in the bar above the keyboard.

import SwiftUI

struct ContentView: View {
    @StateObject private var console = Console()

    var body: some View {
        VStack(spacing: 0) {
            status
            TerminalPane(terminal: console.terminal, onInput: { console.write($0) })
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(Color(Palette.terminalBackground).ignoresSafeArea())
        .preferredColorScheme(.dark)
        .onAppear {
            UIApplication.shared.isIdleTimerDisabled = true
            console.boot()
        }
    }

    // MARK: - Status line

    private var statusText: String {
        switch console.phase {
        case .preparing: return "preparing…"
        case .failed(let message): return message
        case .halted(let code): return code == 0 ? "halted" : "halted (\(code))"
        case .running:
            if let boot = console.bootedAt {
                return String(format: "alpine · up %.1fs", boot)
            }
            return "booting…"
        }
    }

    private var statusColor: Color {
        switch console.phase {
        case .running: return console.bootedAt == nil ? .tokyoYellow : .tokyoGreen
        case .failed: return .tokyoRed
        case .halted: return .tokyoComment
        case .preparing: return .tokyoYellow
        }
    }

    private var status: some View {
        HStack(spacing: 7) {
            Circle()
                .fill(statusColor)
                .frame(width: 6, height: 6)

            Text(statusText)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(Color.tokyoComment)
                .lineLimit(1)
                .truncationMode(.middle)

            Spacer(minLength: 6)

            if case .running = console.phase, console.mips > 0 {
                Text(String(format: "%.0f mips", console.mips))
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(Color.tokyoComment)
                    .contentTransition(.numericText())
            }

            Button {
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                if case .running = console.phase { console.shutdown() } else { console.boot() }
            } label: {
                Image(systemName: "power")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Color.tokyoComment)
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, Layout.terminalInset)
        .padding(.vertical, 7)
        .overlay(alignment: .bottom) {
            Rectangle().fill(Color.tokyoBorder).frame(height: Layout.hairline)
        }
    }
}
