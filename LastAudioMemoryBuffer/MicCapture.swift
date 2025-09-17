//
//  MicCapture.swift
//  LastAudioMemoryBuffer
//
//  Created by Miraj Bhattarai on 17/9/2025.
//

import SwiftUI
import ServiceManagement
import Foundation
import AVFoundation
import AppKit

@MainActor
final class MicCapture: ObservableObject {
    enum PermissionState: CustomStringConvertible {
        case undetermined, denied, granted
        var description: String {
            switch self {
            case .undetermined: return "undetermined"
            case .denied: return "denied"
            case .granted: return "granted"
            }
        }
    }

    @Published var permission: PermissionState = .undetermined
    @Published var isRunning: Bool = false

    private let engine = AVAudioEngine()
    private var ringBuffer = [Float]()

    // Will be set from the input node's format at start()
    private var currentSampleRate: Double = 44100
    private let secondsKept: Double = 30
    private var maxSamples: Int { Int(currentSampleRate * secondsKept) }

    init() {
        Task { await checkPermission() }
    }

    func checkPermission() async {
        let status = AVCaptureDevice.authorizationStatus(for: .audio)
        switch status {
        case .notDetermined:
            let granted = await AVCaptureDevice.requestAccess(for: .audio)
            permission = granted ? .granted : .denied
        case .restricted, .denied:
            permission = .denied
        case .authorized:
            permission = .granted
        @unknown default:
            permission = .denied
        }
    }

    func start() {
        guard permission == .granted else { return }
        guard !isRunning else { return }

        let input = engine.inputNode
        let format = input.inputFormat(forBus: 0)
        currentSampleRate = format.sampleRate

        input.installTap(onBus: 0, bufferSize: 2048, format: format) { [weak self] (buffer, _) in
            guard let self else { return }

            // Pull mono (channel 0). Many mics are mono; if stereo, we just take L.
            guard let channels = buffer.floatChannelData else { return }
            let frameCount = Int(buffer.frameLength)
            let channel0 = channels[0]
            let samples = Array(UnsafeBufferPointer(start: channel0, count: frameCount))
            self.appendToRingBuffer(samples)
        }

        do {
            try engine.start()
            isRunning = true
        } catch {
            print("Engine start failed: \(error)")
        }
    }

    func stop() {
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        isRunning = false
    }

    private func appendToRingBuffer(_ samples: [Float]) {
        ringBuffer.append(contentsOf: samples)
        if ringBuffer.count > maxSamples {
            ringBuffer.removeFirst(ringBuffer.count - maxSamples)
        }
    }

    // MARK: - Saving (sandbox-friendly: user picks the location each time)
    func saveRollingWindowToFile() {
        let savePanel = NSSavePanel()
        savePanel.allowedContentTypes = [.aiff]  // or .wav (use UTType.wav)
        savePanel.nameFieldStringValue = "clip-\(Int(Date().timeIntervalSince1970)).aiff"
        savePanel.begin { [ring = self.ringBuffer, sr = self.currentSampleRate] response in
            guard response == .OK, let url = savePanel.url else { return }
            self.writeAIFF(url: url, samples: ring, sampleRate: sr)
        }
    }

    private func writeAIFF(url: URL, samples: [Float], sampleRate: Double) {
        // Write mono float32
        guard let format = AVAudioFormat(standardFormatWithSampleRate: sampleRate, channels: 1) else { return }
        do {
            let file = try AVAudioFile(forWriting: url,
                                       settings: format.settings,
                                       commonFormat: .pcmFormatFloat32,
                                       interleaved: false)
            guard let buffer = AVAudioPCMBuffer(pcmFormat: format,
                                                frameCapacity: AVAudioFrameCount(samples.count)) else { return }
            buffer.frameLength = buffer.frameCapacity
            samples.withUnsafeBufferPointer { src in
                buffer.floatChannelData![0].update(from: src.baseAddress!, count: samples.count)
            }
            try file.write(from: buffer)
        } catch {
            print("Write failed: \(error)")
        }
    }
}
