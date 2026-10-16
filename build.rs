fn main() {
    println!("cargo:rerun-if-changed=assets/winbaobab.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if !std::path::Path::new("assets/winbaobab.ico").exists() {
        panic!("assets/winbaobab.ico missing; run cargo run --example make_icon");
    }
    // Embeds the icon and version info into the exe (Windows resource section).
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/winbaobab.ico");
    res.set("ProductName", "WinBaobab");
    res.set("FileDescription", "WinBaobab - disk usage analyzer");
    res.set("LegalCopyright", "GPL-2.0-or-later. Unofficial; not affiliated with GNOME.");
    res.set("OriginalFilename", "winbaobab.exe");
    res.compile().expect("embed Windows resources");
}
