// Tokyo Night palette, shared by the terminal and the app chrome.

import SwiftUI
import UIKit

enum Palette {
    static let background = UIColor(red: 0.102, green: 0.106, blue: 0.149, alpha: 1)      // #1A1B26
    static let terminalBackground = UIColor(red: 0.086, green: 0.086, blue: 0.122, alpha: 1) // #16161E
    static let terminalBlack = UIColor(red: 0.255, green: 0.282, blue: 0.408, alpha: 1)    // #414868
    static let highlighted = UIColor(red: 0.161, green: 0.180, blue: 0.259, alpha: 1)      // #292E42
    static let selection = UIColor(red: 0.149, green: 0.173, blue: 0.267, alpha: 1)        // #262B44
    static let foreground = UIColor(red: 0.753, green: 0.792, blue: 0.961, alpha: 1)       // #C0CAF5
    static let comment = UIColor(red: 0.337, green: 0.373, blue: 0.537, alpha: 1)          // #565F89
    static let blue = UIColor(red: 0.478, green: 0.635, blue: 0.969, alpha: 1)             // #7AA2F7
    static let cyan = UIColor(red: 0.490, green: 0.812, blue: 1.0, alpha: 1)               // #7DCFFF
    static let green = UIColor(red: 0.620, green: 0.808, blue: 0.416, alpha: 1)            // #9ECE6A
    static let yellow = UIColor(red: 0.878, green: 0.686, blue: 0.408, alpha: 1)           // #E0AF68
    static let orange = UIColor(red: 1.0, green: 0.620, blue: 0.392, alpha: 1)             // #FF9E64
    static let red = UIColor(red: 0.969, green: 0.463, blue: 0.557, alpha: 1)              // #F7768E
    static let brightRed = UIColor(red: 1.0, green: 0.478, blue: 0.576, alpha: 1)          // #FF7A93
    static let magenta = UIColor(red: 0.733, green: 0.604, blue: 0.969, alpha: 1)          // #BB9AF7
    static let purple = UIColor(red: 0.616, green: 0.486, blue: 0.847, alpha: 1)           // #9D7CD8
    static let brightBlue = UIColor(red: 0.490, green: 0.651, blue: 1.0, alpha: 1)         // #7DA6FF
    static let brightCyan = UIColor(red: 0.051, green: 0.725, blue: 0.843, alpha: 1)       // #0DB9D7
    static let brightGreen = UIColor(red: 0.725, green: 0.949, blue: 0.486, alpha: 1)      // #B9F27C
}

extension Palette {
    /// The 16 ANSI colours the terminal maps SGR codes to.
    static let ansi: [UIColor] = [
        terminalBlack, red, green, yellow, blue, magenta, cyan, foreground,
        comment, brightRed, brightGreen, orange, brightBlue, magenta, brightCyan, .white,
    ]
}

extension Color {
    static let tokyoBackground = Color(Palette.background)
    static let tokyoPanel = Color(Palette.terminalBackground)
    static let tokyoSurface = Color(Palette.highlighted)
    static let tokyoBorder = Color(Palette.highlighted)
    static let tokyoForeground = Color(Palette.foreground)
    static let tokyoComment = Color(Palette.comment)
    static let tokyoBlue = Color(Palette.blue)
    static let tokyoCyan = Color(Palette.cyan)
    static let tokyoGreen = Color(Palette.green)
    static let tokyoYellow = Color(Palette.yellow)
    static let tokyoOrange = Color(Palette.orange)
    static let tokyoRed = Color(Palette.red)
    static let tokyoMagenta = Color(Palette.magenta)
}
