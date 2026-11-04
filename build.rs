// Windows: App-Icon und Versionsinfo in die .exe einbetten.
fn main() {
    println!("cargo:rerun-if-changed=assets/icons/app-icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icons/app-icon.ico");
        res.set("ProductName", "FYA");
        res.set("FileDescription", "FYA");
        if let Err(e) = res.compile() {
            println!("cargo:warning=Windows-Ressourcen nicht eingebettet: {e}");
        }
    }
}
