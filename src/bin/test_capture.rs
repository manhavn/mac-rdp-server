use xcap::Monitor;

fn main() {
    println!("🔍 Testing macOS Screen Capture...");
    let monitors = Monitor::all().expect("Failed to get monitors");
    for (i, m) in monitors.iter().enumerate() {
        println!(
            "📺 Monitor #{}: {}x{}, name: {}",
            i,
            m.width().unwrap_or(0),
            m.height().unwrap_or(0),
            m.name().unwrap_or_default()
        );
        match m.capture_image() {
            Ok(img) => {
                let raw = img.as_raw();
                let non_zero = raw.iter().filter(|&&b| b > 0).count();
                println!(
                    "   📸 Captured image: {}x{}, total bytes: {}, non-zero bytes: {} ({}%)",
                    img.width(),
                    img.height(),
                    raw.len(),
                    non_zero,
                    (non_zero * 100) / raw.len().max(1)
                );
                println!("   Sample first 32 bytes: {:?}", &raw[..raw.len().min(32)]);
            }
            Err(e) => {
                println!("   ❌ Capture failed: {:?}", e);
            }
        }
    }
}
