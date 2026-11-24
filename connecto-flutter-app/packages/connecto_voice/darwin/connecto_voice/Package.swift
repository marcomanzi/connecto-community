// swift-tools-version: 5.9

import PackageDescription

let package = Package(
  name: "connecto_voice",
  platforms: [
    .iOS("17.0"),
    .macOS("14.0"),
  ],
  products: [
    .library(name: "connecto-voice", targets: ["connecto_voice"])
  ],
  dependencies: [
    // PocketTTS (the natural voice) on Core ML. Pinned: its API moves fast.
    .package(url: "https://github.com/FluidInference/FluidAudio.git", revision: "e0c6103bc87a88d25276cad491c86e757f58c865"),
    // llama.cpp (Metal) for the on-phone assistant. Pinned to b10442, the
    // last build whose XCFramework still has an iOS Simulator slice (they
    // stopped with ggml-org/llama.cpp#27252), so the app builds there too;
    // it has Qwen3.5.
    .package(url: "https://github.com/mattt/llama.swift", exact: "2.10442.0"),
  ],
  targets: [
    .target(
      name: "connecto_voice",
      dependencies: [
        .product(name: "FluidAudio", package: "FluidAudio"),
        .product(name: "LlamaSwift", package: "llama.swift"),
      ],
      // Smart Turn v3.2 (Pipecat, BSD-2-Clause), compiled for Core ML: tells
      // a finished turn from a pause mid-sentence, on the device.
      resources: [
        .copy("Resources/smart_turn.mlmodelc"),
        .copy("Resources/smart_turn-LICENSE.txt"),
      ])
  ]
)
