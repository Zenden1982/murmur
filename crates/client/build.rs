//! Иконка и сведения о программе в exe (ресурсы Windows).
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        #[cfg(windows)]
        {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico")
                .set("ProductName", "Murmur")
                .set("FileDescription", "Murmur")
                .set("CompanyName", "Murmur");
            if let Err(e) = res.compile() {
                // без иконки exe всё равно рабочий
                println!("cargo:warning=иконка exe не встроена: {e}");
            }
        }
    }
}
