//! SVG-иконки (линии) → залитые контуры (SVG path d=...), по строке на файл: "имя<TAB>d".
use std::fs;
use tiny_skia_path::{PathSegment, Stroke, LineCap, LineJoin};

fn main() {
    let dir = std::env::args().nth(1).expect("dir");
    let mut names: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "svg")).collect();
    names.sort();
    for p in names {
        let data = fs::read(&p).unwrap();
        let tree = usvg::Tree::from_data(&data, &usvg::Options::default()).unwrap();
        let mut d = String::new();
        collect(tree.root(), &mut d);
        println!("{}\t{}", p.file_stem().unwrap().to_string_lossy(), d.trim());
    }
}

fn collect(g: &usvg::Group, d: &mut String) {
    for n in g.children() {
        match n {
            usvg::Node::Group(g) => collect(g, d),
            usvg::Node::Path(path) => {
                let ts = path.abs_transform();
                let src = path.data().clone().transform(ts).unwrap();
                if let Some(s) = path.stroke() {
                    let st = Stroke { width: s.width().get(), line_cap: LineCap::Round, line_join: LineJoin::Round, miter_limit: 4.0, dash: None };
                    if let Some(out) = src.stroke(&st, 8.0) { emit(&out, d); }
                }
                if path.fill().is_some() { emit(&src, d); }
            }
            _ => {}
        }
    }
}

fn emit(p: &tiny_skia_path::Path, d: &mut String) {
    for seg in p.segments() {
        match seg {
            PathSegment::MoveTo(a) => d.push_str(&format!("M{} {} ", a.x, a.y)),
            PathSegment::LineTo(a) => d.push_str(&format!("L{} {} ", a.x, a.y)),
            PathSegment::QuadTo(a, b) => d.push_str(&format!("Q{} {} {} {} ", a.x, a.y, b.x, b.y)),
            PathSegment::CubicTo(a, b, c) => d.push_str(&format!("C{} {} {} {} {} {} ", a.x, a.y, b.x, b.y, c.x, c.y)),
            PathSegment::Close => d.push_str("Z "),
        }
    }
}
