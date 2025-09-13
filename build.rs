fn main() {
    let mut res = winres::WindowsResource::new();
    res.set_icon("AudioFileConverter.ico");
    res.compile().unwrap();
}