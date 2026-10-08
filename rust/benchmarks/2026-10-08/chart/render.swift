// Native, dependency-free macOS PNG export of the same scene as comparison.svg.
import AppKit
import Foundation

let data = try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1]))
let scene = try JSONSerialization.jsonObject(with: data) as! [String: Any]
let width = (scene["width"] as! NSNumber).doubleValue
let height = (scene["height"] as! NSNumber).doubleValue
func number(_ row: [String: Any], _ key: String) -> CGFloat { CGFloat((row[key] as! NSNumber).doubleValue) }
func color(_ value: String) -> NSColor {
    let n = UInt32(value.dropFirst(), radix: 16)!
    return NSColor(srgbRed: CGFloat((n >> 16) & 255)/255, green: CGFloat((n >> 8) & 255)/255, blue: CGFloat(n & 255)/255, alpha: 1)
}
let image = NSImage(size: NSSize(width: width, height: height))
image.lockFocusFlipped(true)
for row in scene["items"] as! [[String: Any]] {
    switch row["kind"] as! String {
    case "rect":
        let rect = NSRect(x: number(row,"x"), y: number(row,"y"), width: number(row,"width"), height: number(row,"height"))
        let path = NSBezierPath(roundedRect: rect, xRadius: number(row,"radius"), yRadius: number(row,"radius"))
        color(row["fill"] as! String).setFill(); path.fill()
        if let stroke = row["stroke"] as? String {color(stroke).setStroke();path.lineWidth=1;path.stroke()}
    case "line":
        let path = NSBezierPath();path.move(to: NSPoint(x:number(row,"x1"),y:number(row,"y1")));path.line(to:NSPoint(x:number(row,"x2"),y:number(row,"y2")));color(row["fill"] as! String).setStroke();path.lineWidth=1;path.stroke()
    default:
        let weight = (row["weight"] as! NSNumber).intValue
        let fontName = weight >= 600 ? "HelveticaNeue-Bold" : (weight >= 500 ? "HelveticaNeue-Medium" : "HelveticaNeue")
        let font = NSFont(name:fontName,size:number(row,"size"))!
        let string = NSAttributedString(string:row["text"] as! String,attributes:[.font:font,.foregroundColor:color(row["fill"] as! String),.kern:number(row,"kern")])
        let x = number(row,"x") - ((row["align"] as! String) == "right" ? string.size().width : 0)
        string.draw(at:NSPoint(x:x,y:number(row,"y")))
    }
}
image.unlockFocus()
let representation = NSBitmapImageRep(cgImage:image.cgImage(forProposedRect:nil,context:nil,hints:nil)!)
let png = representation.representation(using:.png,properties:[:])!
try png.write(to:URL(fileURLWithPath:CommandLine.arguments[2]),options:.atomic)
