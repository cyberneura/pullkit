// Prepare distinct @1x list artwork and padded larger artwork from a fixed master.
// Usage: swift scripts/prepare-macos-icons.swift resources/app-icons/layout.json
import AppKit
import ImageIO

struct Layout: Decodable {
    let source: String
    let body: [Double]
    let glyph: [Double]
    let radius: Double
    let whiteGradientGlyph: Bool?
}
let configURL = URL(fileURLWithPath: CommandLine.arguments[1]).standardizedFileURL
let directory = configURL.deletingLastPathComponent()
let layout = try JSONDecoder().decode(Layout.self, from: Data(contentsOf: configURL))
let sourceURL = directory.appendingPathComponent(layout.source)
// ImageIO preserves embedded profiles and interprets untagged RGB as sRGB.
// Drawing into explicit sRGB buffers converts profiled artwork without retagging it.
let imageSource = CGImageSourceCreateWithURL(sourceURL as CFURL, nil)!
let original = CGImageSourceCreateImageAtIndex(imageSource, 0, nil)!
let source = NSBitmapImageRep(cgImage: original)
func rectangle(_ v: [Double]) -> CGRect { CGRect(x: v[0], y: v[1], width: v[2], height: v[3]) }
let body = rectangle(layout.body)
let glyph = rectangle(layout.glyph)
// Draw a source pixel/strip to preserve its actual color profile instead of
// treating NSBitmapImageRep.colorAt's calibrated components as sRGB.
let sample = CGRect(x: floor(body.minX+body.width*0.08),
                    y: layout.whiteGradientGlyph == true ? body.minY : floor(body.midY),
                    width: 1, height: layout.whiteGradientGlyph == true ? body.height : 1)
var backgroundImage = NSImage(cgImage: original.cropping(to: sample)!, size: sample.size)
if layout.whiteGradientGlyph == true {
    // Sample each row away from both the rounded corners and the white mark.
    let h = Int(body.height)
    let strip = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 1, pixelsHigh: h,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 4, bitsPerPixel: 32)!.retagging(with: .sRGB)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: strip)
    for y in 0..<h {
        let sy = Int(body.minY)+y
        let sx = Double(sy) < glyph.minY || Double(sy) >= glyph.maxY
            ? Int(body.midX) : Int(body.minX+body.width*0.08)
        let pixel = NSImage(cgImage: original.cropping(to: CGRect(x: sx,y: sy,width: 1,height: 1))!,
                            size: NSSize(width: 1,height: 1))
        pixel.draw(in: CGRect(x: 0,y: h-y-1,width: 1,height: 1),from: .zero,operation: .copy,fraction: 1)
    }
    NSGraphicsContext.restoreGraphicsState()
    backgroundImage = NSImage(size: strip.size)
    backgroundImage.addRepresentation(strip)
}
let plateImage = NSImage(cgImage: original.cropping(to: body)!, size: body.size)
var glyphImage = NSImage(cgImage: original.cropping(to: glyph)!, size: glyph.size)
// A gradient cannot be cropped together with its glyph: scaling that rectangle
// would leave a differently shaded patch. Recover the existing white mark's alpha.
if layout.whiteGradientGlyph == true {
    let mask = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(glyph.width),
        pixelsHigh: Int(glyph.height), bitsPerSample: 8, samplesPerPixel: 4,
        hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
        bytesPerRow: Int(glyph.width)*4, bitsPerPixel: 32)!.retagging(with: .sRGB)!
    for y in 0..<Int(glyph.height) {
        let sy = Int(glyph.minY)+y
        let bg = source.colorAt(x: Int(body.minX+body.width*0.08), y: sy)!
        for x in 0..<Int(glyph.width) {
            let c = source.colorAt(x: Int(glyph.minX)+x,y: sy)!
            let alpha = max(0, min(1, (c.redComponent-bg.redComponent)/(1-bg.redComponent)))
            mask.setColor(NSColor(deviceRed: 1, green: 1, blue: 1, alpha: alpha), atX: x, y: y)
        }
    }
    glyphImage = NSImage(cgImage: mask.cgImage!, size: glyph.size)
}
func canvas(_ size: Int, small: Bool) -> NSBitmapImageRep {
    let b = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: size*4, bitsPerPixel: 32)!.retagging(with: .sRGB)!
    b.bitmapData!.initialize(repeating: 0, count: b.bytesPerRow*size)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: b)
    NSGraphicsContext.current!.imageInterpolation = .high
    let n = Double(size)
    if small {
        let rect = CGRect(x: 0,y: 0,width: n,height: n)
        NSBezierPath(roundedRect: rect, xRadius: n*layout.radius, yRadius: n*layout.radius).addClip()
        backgroundImage.draw(in: rect, from: .zero, operation: .copy, fraction: 1)
        let scale = n*0.8/max(glyph.width,glyph.height)
        let w=glyph.width*scale, h=glyph.height*scale
        glyphImage.draw(in: CGRect(x:(n-w)/2,y:(n-h)/2,width:w,height:h),
                        from:.zero,operation:.sourceOver,fraction:1)
    } else {
        plateImage.draw(in:CGRect(x:n*0.1,y:n*0.1,width:n*0.8,height:n*0.8),
                        from:.zero,operation:.copy,fraction:1)
    }
    NSGraphicsContext.restoreGraphicsState()
    return b
}
let iconset = directory.appendingPathComponent("AppIcon.iconset")
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for (pt,scale) in [(16,1),(16,2),(32,1),(32,2),(128,1),(128,2),(256,1),(256,2),(512,1),(512,2)] {
    let n=pt*scale
    let small=scale==1 && pt<=32
    let rendered=canvas(small ? n*4 : n,small:small)
    let image=NSImage(cgImage:rendered.cgImage!,size:NSSize(width:n,height:n))
    let out=NSBitmapImageRep(bitmapDataPlanes:nil,pixelsWide:n,pixelsHigh:n,bitsPerSample:8,
        samplesPerPixel:4,hasAlpha:true,isPlanar:false,colorSpaceName:.deviceRGB,bytesPerRow:n*4,bitsPerPixel:32)!.retagging(with: .sRGB)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current=NSGraphicsContext(bitmapImageRep:out)
    NSGraphicsContext.current!.imageInterpolation = .high
    image.draw(in:CGRect(x:0,y:0,width:n,height:n),from:.zero,operation:.copy,fraction:1)
    NSGraphicsContext.restoreGraphicsState()
    // A barely translucent antialiased outer edge prevents macOS 27 from
    // treating the opaque plate as an inset picture and adding a white frame.
    // No transparent band is added: the background still reaches every edge.
    // A PNG encode/decode round trip reintroduces incorrect gAMA=2.2 on export.
    let final = out
    if small {
        for y in 0..<n { for x in 0..<n where x==0 || y==0 || x==n-1 || y==n-1 {
            var pixel=[UInt](repeating: 0,count: 4)
            final.getPixel(&pixel,atX: x,y: y)
            let alpha=min(239,pixel[3])
            if !final.bitmapFormat.contains(.alphaNonpremultiplied) && pixel[3]>0 {
                for channel in 0..<3 { pixel[channel]=pixel[channel]*alpha/pixel[3] }
            }
            pixel[3]=alpha
            final.setPixel(&pixel,atX: x,y: y)
        } }
    }
    let suffix=scale==2 ? "@2x" : ""
    try final.representation(using:.png,properties:[:])!.write(to:iconset.appendingPathComponent("icon_\(pt)x\(pt)\(suffix).png"))
}
print("Prepared \(iconset.path)")
