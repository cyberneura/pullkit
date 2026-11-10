import AppKit
import Carbon

var inputDirectory = URL(fileURLWithPath: ".")
var iconName = "AppIcon"
let slots: [(type: String, pt: Int, scale: Int)] = [
    ("ic10", 512, 2), ("ic09", 512, 1), ("ic14", 256, 2),
    ("ic08", 256, 1), ("ic13", 128, 2), ("ic07", 128, 1),
    ("ic12", 32, 2), ("ic11", 16, 2), ("ic05", 32, 1), ("ic04", 16, 1)
]

// Inspect the PNG metadata independently of AppKit. AppKit-to-AppKit pixel
// comparisons can miss gAMA=2.2, which other decoders display too brightly.
func validatePNGColorSpace(_ data: Data, name: String) throws {
    guard data.prefix(8) == Data([137, 80, 78, 71, 13, 10, 26, 10]) else {
        throw IconError("Expected PNG: \(name)")
    }
    var offset = 8
    var srgb = false
    while offset < data.count {
        guard data.count - offset >= 12 else { throw IconError("Truncated PNG: \(name)") }
        let length = data[offset..<offset + 4].reduce(0) { ($0 << 8) | Int($1) }
        guard length <= data.count - offset - 12 else { throw IconError("Truncated PNG chunk: \(name)") }
        let type = String(decoding: data[offset + 4..<offset + 8], as: UTF8.self)
        let payload = data.subdata(in: offset + 8..<offset + 8 + length)
        if type == "sRGB" {
            guard length == 1, payload[0] <= 3 else { throw IconError("Invalid sRGB chunk: \(name)") }
            srgb = true
        } else if type == "gAMA" {
            guard payload == Data([0, 0, 177, 143]) else {
                throw IconError("Conflicting PNG gamma; export sRGB (gAMA 45455), not display gamma 2.2: \(name)")
            }
        } else if type == "iCCP" {
            throw IconError("Convert artwork to sRGB before export; do not mix ICC and sRGB tags: \(name)")
        }
        offset += length + 12
    }
    guard srgb else { throw IconError("Missing explicit sRGB PNG tag: \(name)") }
}

func sourceEntries() throws -> [Entry] {
    try slots.map { slot in
        let suffix = slot.scale == 2 ? "@2x" : ""
        let file = inputDirectory.appendingPathComponent("icon_\(slot.pt)x\(slot.pt)\(suffix).png")
        let data = try Data(contentsOf: file)
        try validatePNGColorSpace(data, name: file.lastPathComponent)
        let pixels = slot.pt * slot.scale
        guard let b = NSBitmapImageRep(data: data), b.pixelsWide == pixels, b.pixelsHigh == pixels,
              b.hasAlpha else { throw IconError("Expected \(pixels)x\(pixels) PNG with alpha: \(file.path)") }
        guard data.prefix(8) == Data([137, 80, 78, 71, 13, 10, 26, 10]) else {
            throw IconError("Expected PNG: \(file.path)")
        }
        return Entry(type: slot.type, body: slot.scale == 1 && slot.pt <= 32 ? try argb(data, size: pixels) : data)
    }
}

struct IconError: Error, CustomStringConvertible {
    let description: String
    init(_ message: String) { description = message }
}

struct Entry {
    let type: String
    let body: Data
}

func readEntries(_ data: Data) throws -> [Entry] {
    func uint32(_ offset: Int) -> Int {
        data[offset..<offset + 4].reduce(0) { ($0 << 8) | Int($1) }
    }
    guard data.count >= 8, data.prefix(4) == Data("icns".utf8), uint32(4) == data.count else {
        throw IconError("Invalid ICNS header")
    }
    var entries: [Entry] = []
    var offset = 8
    while offset < data.count {
        guard offset + 8 <= data.count else { throw IconError("Truncated ICNS entry") }
        let length = uint32(offset + 4)
        guard length >= 8, offset + length <= data.count else {
            throw IconError("Invalid ICNS entry length")
        }
        entries.append(Entry(
            type: String(decoding: data[offset..<offset + 4], as: UTF8.self),
            body: data.subdata(in: offset + 8..<offset + length)
        ))
        offset += length
    }
    return entries
}

func chunk(_ type: String, _ body: Data) -> Data {
    var length = UInt32(body.count + 8).bigEndian
    return Data(type.utf8) + withUnsafeBytes(of: &length) { Data($0) } + body
}

func buildCatalog(_ entries: [Entry], beside path: URL) throws {
    let fm = FileManager.default
    let directory = fm.temporaryDirectory.appendingPathComponent("macos-icon-assets-\(UUID().uuidString)")
    let catalog = directory.appendingPathComponent("Icons.xcassets")
    let icons = catalog.appendingPathComponent("\(iconName).appiconset")
    let output = directory.appendingPathComponent("output")
    try fm.createDirectory(at: icons, withIntermediateDirectories: true)
    defer { try? fm.removeItem(at: directory) }
    try fm.createDirectory(at: output, withIntermediateDirectories: true)
    let info: [String: Any] = ["author": "xcode", "version": 1]
    try JSONSerialization.data(withJSONObject: ["info": info]).write(to: catalog.appendingPathComponent("Contents.json"))
    let slots: [(Int, Int, String?)] = [
        (16, 1, nil), (16, 2, "ic11"), (32, 1, nil), (32, 2, "ic12"),
        (128, 1, "ic07"), (128, 2, "ic13"), (256, 1, "ic08"), (256, 2, "ic14"),
        (512, 1, "ic09"), (512, 2, "ic10")
    ]
    var images: [[String: String]] = []
    for (size, scale, slot) in slots {
        let data: Data
        if let slot {
            guard let entry = entries.first(where: { $0.type == slot }) else {
                throw IconError("Missing catalog source: \(slot)")
            }
            data = entry.body
        } else {
            data = try reference(size)
        }
        let filename = "icon-\(size)-\(scale).png"
        try data.write(to: icons.appendingPathComponent(filename))
        images.append(["idiom": "mac", "size": "\(size)x\(size)", "scale": "\(scale)x", "filename": filename])
    }
    try JSONSerialization.data(withJSONObject: ["info": info, "images": images], options: .prettyPrinted)
        .write(to: icons.appendingPathComponent("Contents.json"))
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
    process.arguments = ["actool", catalog.path, "--compile", output.path, "--platform", "macosx",
                         "--minimum-deployment-target", "10.13", "--app-icon", iconName,
                         "--output-partial-info-plist", directory.appendingPathComponent("Info.plist").path]
    try process.run()
    process.waitUntilExit()
    guard process.terminationStatus == 0 else { throw IconError("actool failed: \(process.terminationStatus)") }
    // actool の ICNS は使わず、最大サイズが先頭の既存 ICNS を維持する。
    let destination = path.deletingLastPathComponent().appendingPathComponent("Assets.car")
    try Data(contentsOf: output.appendingPathComponent("Assets.car")).write(to: destination, options: .atomic)
    print("Generated \(destination.path)")
}

func reference(_ size: Int) throws -> Data {
    try Data(contentsOf: inputDirectory.appendingPathComponent("icon_\(size)x\(size).png"))
}

func argb(_ data: Data, size: Int) throws -> Data {
    guard let bitmap = NSBitmapImageRep(data: data), bitmap.pixelsWide == size,
          bitmap.pixelsHigh == size else { throw IconError("Invalid \(size)px reference") }
    var planes = [[UInt8]](repeating: [], count: 4)
    for y in 0..<size {
        for x in 0..<size {
            // Preserve bitmap components; another color-space conversion applies
            // an extra transfer curve when serializing raw ARGB.
            let color = bitmap.colorAt(x: x, y: y)!
            let rgb: [CGFloat]
            switch color.colorSpace.colorSpaceModel {
            case .rgb:
                rgb = [color.redComponent, color.greenComponent, color.blueComponent]
            case .gray:
                // Gray+alpha PNGs return NSCalibratedWhite colors. RGB component
                // accessors throw on those; copy the gray component to each plane.
                rgb = [CGFloat](repeating: color.whiteComponent, count: 3)
            default:
                throw IconError("Small PNG must use RGB or grayscale components")
            }
            for (index, value) in ([color.alphaComponent] + rgb).enumerated() {
                planes[index].append(UInt8((value * 255).rounded()))
            }
        }
    }
    // ARGB の各面を 128 バイト以下のリテラルとして RLE 符号化する。
    // 色はアルファ乗算前の値。PNG を icp4 / icp5 に入れると IconServices で色化けする。
    var body = Data("ARGB".utf8)
    for plane in planes {
        for start in stride(from: 0, to: plane.count, by: 128) {
            let end = min(start + 128, plane.count)
            body.append(UInt8(end - start - 1))
            body.append(contentsOf: plane[start..<end])
        }
    }
    return body
}

func render(_ image: NSImage?, icon: IconRef? = nil, size: Int, scale: Int = 1) throws -> NSBitmapImageRep {
    let pixels = size * scale
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: pixels * 4, bitsPerPixel: 32)!
        .retagging(with: .sRGB)!
    bitmap.bitmapData!.initialize(repeating: 0, count: bitmap.bytesPerRow * pixels)
    bitmap.size = NSSize(width: size, height: size)
    NSGraphicsContext.saveGraphicsState()
    defer { NSGraphicsContext.restoreGraphicsState() }
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
    var rect = CGRect(x: 0, y: 0, width: size, height: size)
    if let icon {
        let status = PlotIconRefInContext(NSGraphicsContext.current!.cgContext, &rect, 0, 0, nil, 0, icon)
        guard status == noErr else { throw IconError("IconServices draw failed: \(status)") }
    } else {
        image!.draw(in: rect, from: .zero, operation: .copy, fraction: 1)
    }
    guard (0..<pixels).contains(where: { y in
        (0..<pixels).contains { x in bitmap.colorAt(x: x, y: y)!.alphaComponent > 0 }
    }) else { throw IconError("Empty native rendering; verify access to macOS IconServices") }
    return bitmap
}

func verify(_ path: URL, entries: [Entry]) throws {
    let fm = FileManager.default
    let data = try Data(contentsOf: path)
    var icon: IconRef?
    // 非推奨 API だが、ImageIO が見逃す小サイズの色化けを再現するデコーダーを
    // 直接通せる。NSWorkspace と違い、OS が付ける枠・影やキャッシュに依存しない。
    let status = data.withUnsafeBytes { raw in
        GetIconRefFromIconFamilyPtr(UnsafePointer(OpaquePointer(raw.baseAddress!)), data.count, &icon)
    }
    guard status == noErr, let icon else { throw IconError("IconServices load failed: \(status)") }
    defer { ReleaseIconRef(icon) }
    // 64pt の専用スロットは無いため、同じ倍率の 128pt 画像を縮小する。
    let normalSlots = [64: "ic07", 128: "ic07", 256: "ic08", 512: "ic09"]
    let retinaSlots = [16: "ic11", 32: "ic12", 64: "ic13", 128: "ic13", 256: "ic14", 512: "ic10"]
    var failures: [String] = []
    for size in [16, 32, 64, 128, 256, 512] {
        for scale in [1, 2] {
            let pixels = size * scale
            let data: Data
            if scale == 1 && size <= 32 {
                data = try reference(pixels)
            } else {
                // 物理 px が同じでも、通常表示と Retina の画像は別に選ぶ。
                let slot = (scale == 1 ? normalSlots : retinaSlots)[size]
                guard let entry = entries.first(where: { $0.type == slot }) else {
                    throw IconError("Missing \(pixels)px slot")
                }
                data = entry.body
            }
            guard let expected = NSImage(data: data) else { throw IconError("Unreadable reference") }
            let a = try render(nil, icon: icon, size: size, scale: scale)
            let b = try render(expected, size: size, scale: scale)
            let count = pixels * pixels * 4
            let error = (0..<count).reduce(0) { $0 + abs(Int(a.bitmapData![$1]) - Int(b.bitmapData![$1])) }
            let meanError = Double(error) / Double(count)
            let label = "\(size)pt @\(scale)x (\(pixels)px)"
            // IconServices の縮小・色変換による差は許容する (各チャンネル 0〜255)。
            // 格納形式に起因する色化けは、小さな縮小・色変換の差より大幅に大きい。
            if meanError <= 8 {
                print("PASS \(label), mean channel error \(meanError)")
            } else {
                failures.append("\(label): mean channel error \(meanError) exceeds 8")
                // 失敗した画像だけ保存し、CI や手元で目視できるようにする。
                let output = URL(fileURLWithPath: ProcessInfo.processInfo.environment["ICON_ARTIFACTS"] ?? NSTemporaryDirectory())
                try fm.createDirectory(at: output, withIntermediateDirectories: true)
                for (name, bitmap) in [("actual", a), ("expected", b)] {
                    try bitmap.representation(using: .png, properties: [:])!
                        .write(to: output.appendingPathComponent("icon-\(size)pt-\(scale)x-\(name).png"))
                }
            }
        }
    }
    guard failures.isEmpty else { throw IconError(failures.joined(separator: "\n")) }
    guard entries.first?.type == "ic10", !entries.contains(where: { ["icp4", "icp5", "icp6"].contains($0.type) }),
          entries.filter({ $0.type == "ic04" }).count == 1,
          entries.filter({ $0.type == "ic05" }).count == 1 else {
        throw IconError("Expected largest-first ICNS with ic04 / ic05 small icons")
    }
}

func verifyCatalog(_ path: URL) throws {
    let fm = FileManager.default
    let directory = fm.temporaryDirectory.appendingPathComponent("macos-icon-apps-\(UUID().uuidString)")
    try fm.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? fm.removeItem(at: directory) }
    let plistData = try Data(contentsOf: path.deletingLastPathComponent().appendingPathComponent("IconInfo.plist"))
    let extra = try PropertyListSerialization.propertyList(from: plistData, format: nil) as! [String: Any]
    guard extra["CFBundleIconName"] as? String == iconName,
          extra["CFBundleIconFile"] as? String == "icon.icns" else {
        throw IconError("IconInfo.plist does not match the catalog name / icon.icns")
    }
    func appIcon(_ name: String, png: Data? = nil, catalog: Bool = false) throws -> NSImage {
        let app = directory.appendingPathComponent("\(name).app")
        let contents = app.appendingPathComponent("Contents")
        try fm.createDirectory(at: contents.appendingPathComponent("Resources"), withIntermediateDirectories: true)
        try fm.createDirectory(at: contents.appendingPathComponent("MacOS"), withIntermediateDirectories: true)
        // アイコンを読み込める最小のアプリ構造を作る。実行はしない。
        try fm.copyItem(at: URL(fileURLWithPath: "/usr/bin/true"), to: contents.appendingPathComponent("MacOS/probe"))
        var plist: [String: Any] = ["CFBundleIdentifier": "com.example.icon-probe.\(UUID().uuidString)",
            "CFBundleName": "IconProbe", "CFBundlePackageType": "APPL", "CFBundleExecutable": "probe",
            "CFBundleIconFile": png == nil ? "icon.icns" : "reference.png"]
        if let png {
            try png.write(to: contents.appendingPathComponent("Resources/reference.png"))
        } else {
            try fm.copyItem(at: path, to: contents.appendingPathComponent("Resources/icon.icns"))
        }
        if catalog {
            plist.merge(extra) { _, new in new }
            try fm.copyItem(at: path.deletingLastPathComponent().appendingPathComponent("Assets.car"),
                            to: contents.appendingPathComponent("Resources/Assets.car"))
        }
        try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
            .write(to: contents.appendingPathComponent("Info.plist"))
        return NSWorkspace.shared.icon(forFile: app.path)
    }
    let actual = try appIcon("Actual", catalog: true)
    let fallback = try appIcon("Fallback")
    var failures: [String] = []
    // 小さい一覧では PNG と同じ全面表示、大きい表示では同じ ICNS と描画が一致することを求める。
    for (size, scale) in [(16, 1), (20, 1), (32, 1), (48, 2), (64, 2), (128, 1), (128, 2)] {
        let expected = size <= 32 ? try appIcon("Reference-\(size)", png: reference(size == 16 ? 16 : 32)) : fallback
        let a = try render(actual, size: size, scale: scale)
        let b = try render(expected, size: size, scale: scale)
        let count = a.bytesPerRow * a.pixelsHigh
        let error = Double((0..<count).reduce(0) { $0 + abs(Int(a.bitmapData![$1]) - Int(b.bitmapData![$1])) }) / Double(count)
        let label = "App \(size)pt @\(scale)x"
        // A PNG-reference app can receive the same unwanted white frame, so
        // equality with that app alone is insufficient. Compare the actual
        // plate edge with a direct render of the source background as well.
        if scale == 1 && size <= 32 {
            let source = try render(NSImage(data: reference(size == 16 ? 16 : 32)), size: size)
            guard let edgeX = (0..<size/2).first(where: {
                a.colorAt(x: $0, y: size/2)!.alphaComponent >= 0.85
            }) else {
                throw IconError("\(label): no background edge with alpha >= 0.85; use an opaque background plate")
            }
            let actualEdge = a.colorAt(x: edgeX, y: size/2)!
            let sourceEdge = source.colorAt(x: 0, y: size/2)!
            let actualRGB = [actualEdge.redComponent, actualEdge.greenComponent, actualEdge.blueComponent]
            let sourceRGB = [sourceEdge.redComponent, sourceEdge.greenComponent, sourceEdge.blueComponent]
            let brightening = zip(actualRGB,sourceRGB).map { $0-$1 }.max()!
            if brightening > 0.25 {
                failures.append("\(label): native background edge acquired a light frame")
            } else {
                print("PASS \(label) background edge")
            }
        }
        if error <= 2 {
            print("PASS \(label), mean channel error \(error)")
        } else {
            failures.append("\(label): mean channel error \(error) exceeds 2")
        }
        if error > 2 || ProcessInfo.processInfo.environment["ICON_ARTIFACTS"] != nil {
            let output = URL(fileURLWithPath: ProcessInfo.processInfo.environment["ICON_ARTIFACTS"] ?? NSTemporaryDirectory())
            try fm.createDirectory(at: output, withIntermediateDirectories: true)
            for (name, bitmap) in [("actual", a), ("expected", b)] {
                try bitmap.representation(using: .png, properties: [:])!
                    .write(to: output.appendingPathComponent("app-icon-\(size)pt-\(scale)x-\(name).png"))
            }
        }
    }
    guard failures.isEmpty else { throw IconError(failures.joined(separator: "\n")) }
}

func inspectApp(_ app: URL, output: URL) throws {
    guard FileManager.default.fileExists(atPath: app.appendingPathComponent("Contents/Info.plist").path) else {
        throw IconError("Not an app bundle: \(app.path)")
    }
    try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
    let icon = NSWorkspace.shared.icon(forFile: app.path)
    for (size, scale) in [(16, 1), (16, 2), (20, 1), (20, 2), (32, 1), (32, 2), (64, 2), (128, 2)] {
        let b = try render(icon, size: size, scale: scale)
        try b.representation(using: .png, properties: [:])!
            .write(to: output.appendingPathComponent("icon-\(size)pt-\(scale)x.png"))
        var left = b.pixelsWide, top = b.pixelsHigh, right = -1, bottom = -1
        for y in 0..<b.pixelsHigh { for x in 0..<b.pixelsWide {
            if b.colorAt(x: x, y: y)!.alphaComponent >= 0.5 {
                left = min(left, x); top = min(top, y); right = max(right, x); bottom = max(bottom, y)
            }
        }}
        guard right >= left else { throw IconError("No opaque icon in native rendering") }
        print("\(size)pt @\(scale)x: canvas \(b.pixelsWide)px, alpha>=0.5 margins L/T/R/B: " +
              "\(left)/\(top)/\(b.pixelsWide-right-1)/\(b.pixelsHigh-bottom-1)px")
    }
}

do {
    let args = Array(CommandLine.arguments.dropFirst())
    if args.count == 3 && args[0] == "render" {
        try inspectApp(URL(fileURLWithPath: args[1]), output: URL(fileURLWithPath: args[2]))
    } else {
        guard args.count == 4, ["build", "verify"].contains(args[0]) else {
            throw IconError("Usage: swift macos-icon.swift build|verify INPUT_ICONSET OUTPUT_DIR CATALOG_NAME\n" +
                            "       swift macos-icon.swift render APP_PATH OUTPUT_DIR")
        }
        inputDirectory = URL(fileURLWithPath: args[1])
        let output = URL(fileURLWithPath: args[2])
        iconName = args[3]
        guard iconName.range(of: "^[A-Za-z][A-Za-z0-9_-]*$", options: .regularExpression) != nil else {
            throw IconError("Catalog name must start with a letter and contain only ASCII letters, digits, _ or -")
        }
        let expected = try sourceEntries()
        let path = output.appendingPathComponent("icon.icns")
        if args[0] == "build" {
            try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
            try chunk("icns", expected.reduce(Data()) { $0 + chunk($1.type, $1.body) }).write(to: path, options: .atomic)
            try buildCatalog(expected, beside: path)
            let plist = ["CFBundleIconName": iconName, "CFBundleIconFile": "icon.icns"]
            try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
                .write(to: output.appendingPathComponent("IconInfo.plist"))
        }
        let actual = try readEntries(Data(contentsOf: path))
        guard actual.count == expected.count,
              zip(actual, expected).allSatisfy({ pair in pair.0.type == pair.1.type && pair.0.body == pair.1.body }) else {
            throw IconError("ICNS differs from the input iconset (slots, order or image data)")
        }
        try verify(path, entries: actual)
        try verifyCatalog(path)
    }
} catch {
    fputs("\(error)\n", stderr)
    exit(1)
}
