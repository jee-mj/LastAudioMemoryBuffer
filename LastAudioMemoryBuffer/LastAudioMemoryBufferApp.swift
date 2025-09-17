//
//  LastAudioMemoryBufferApp.swift
//  LastAudioMemoryBuffer
//
//  Created by Miraj Bhattarai on 17/9/2025.
//

import SwiftUI
import ServiceManagement
import AVFoundation
import AppKit

@main
struct LastAudioMemoryBuffer: App {
    @StateObject private var audio = MicCapture()

    var body: some Scene {
        // This creates the menu bar icon + menu
        MenuBarExtra("SonicBuffer", systemImage: "waveform") {
            VStack(alignment: .leading, spacing: 8) {
                Text(audio.isRunning ? "Recording…" : "Stopped")
                    .font(.headline)
                Text("Input permission: \(audio.permission.description)")
                    .font(.caption)

                Divider()

                Button(audio.isRunning ? "Stop" : "Start") {
                    audio.isRunning ? audio.stop() : audio.start()
                }

                Button("Save last 30s to file…") {
                    audio.saveRollingWindowToFile()
                }

                Divider()

                ToggleLaunchAtLoginView()

                Divider()

                Button("Quit") {
                    NSApplication.shared.terminate(nil)
                }
            }
            .padding(8)
            .frame(width: 260)
        }
        // No default window scenes; this is enough for a menu bar app
    }
}
