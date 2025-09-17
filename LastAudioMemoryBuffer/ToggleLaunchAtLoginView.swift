//
//  ToggleLaunchAtLoginView.swift
//  LastAudioMemoryBuffer
//
//  Created by Miraj Bhattarai on 17/9/2025.
//


import SwiftUI
import ServiceManagement

struct ToggleLaunchAtLoginView: View {
    @State private var isEnabled: Bool = (SMAppService.mainApp.status == .enabled)
    @State private var error: String?

    var body: some View {
        Toggle("Launch at Login", isOn: $isEnabled)
            .onChange(of: isEnabled) {
                do {
                    if isEnabled {
                        try SMAppService.mainApp.register()
                    } else {
                        try SMAppService.mainApp.unregister()
                    }
                    error = nil
                } catch {
                    // revert UI on failure
                    isEnabled.toggle()
                    self.error = error.localizedDescription
                }
            }
            .help(error ?? "")
    }
}
