# LastAudioMemoryBuffer 🐑

A personal audio time machine.

## Overview

LAMB is a lightweight macOS menu bar utility that continuously records audio into a rolling memory buffer. Think of it as a "last 30 minutes button" for your studio: when inspiration strikes and you forgot to hit record, LAMB has you covered.

When you click **Recall**, LAMB instantly writes the last 30 minutes of audio to disk as clean, lossless WAV files. Each connected input interface is preserved exactly as you configured it.

## Features

* Continuous audio buffering in memory (no noisy temp files cluttering your disk).
* Configurable input sources on first run.
* Supports multiple audio interfaces, including aggregate devices.
* 32-bit, 44.1 kHz PCM WAV output per input channel or stereo pair.
* Instant **Recall** action with OS file save prompt.
* Minimal resource usage, designed for 24/7 background operation.

## Why LAMB?

Traditional DAWs only record if you *remember* to hit record. LAMB flips the paradigm: it records everything in the background so you never lose a performance, a riff, or that fleeting stroke of genius.

## Installation

Currently, LAMB is in active development. Build instructions:

1. Clone the repo:

   ```bash
   git clone https://github.com/jee-mj/LastAudioMemoryBuffer.git
   cd LastAudioMemoryBuffer
   ```
2. Open the Xcode project.
3. Build & run.

## Usage

1. On first launch, select the input interfaces to monitor.
2. LAMB will reserve the required memory and begin buffering.
3. Click **Recall** in the menu bar to save the last 30 minutes of audio.
4. Choose where to save your files—LAMB will create clean WAV files for each configured input.

## Roadmap

* [ ] Adjustable buffer length.
* [ ] Configurable output format (sample rate, bit depth).
* [ ] Hotkey support for Recall.
* [ ] Automatic session organization.
* [ ] Cross-platform exploration (Linux first).

## License

LAMB is free and open source under the GPL-3.0 License.