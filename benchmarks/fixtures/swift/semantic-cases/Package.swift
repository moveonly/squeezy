// swift-tools-version:5.10
import PackageDescription

let package = Package(
    name: "SemanticCases",
    products: [
        .library(name: "Networking", targets: ["Networking"]),
        .library(name: "Storage", targets: ["Storage"]),
        .library(name: "Models", targets: ["Models"]),
        .library(name: "Extensions", targets: ["Extensions"]),
    ],
    targets: [
        .target(name: "Networking", dependencies: ["Models", "Extensions"]),
        .target(name: "Storage", dependencies: ["Models"]),
        .target(name: "Models"),
        .target(name: "Extensions"),
    ]
)
