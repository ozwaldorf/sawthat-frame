//! Render a JPEG to horizontal and vertical PNGs for visual comparison.
//!
//! Usage: render <image.jpg> <band> <date DD-MM-YYYY> <venue> <out-prefix>

use std::{env, fs, io::BufWriter, time::Instant};

use sawthat_render::{
    ConcertInfo, HORIZ_SIZE, PackedImage, Renderer, VERT_SIZE, concerts::format_date, decode_jpeg,
    palette::PALETTE, primary_color,
};

fn main() {
    let args: Vec<String> = env::args().collect();
    let [_, path, band, date, venue, out] = args.as_slice() else {
        eprintln!("usage: render <image.jpg> <band> <DD-MM-YYYY> <venue> <out-prefix>");
        std::process::exit(2);
    };

    let t = Instant::now();
    let src = decode_jpeg(&fs::read(path).unwrap()).expect("decode");
    println!("decode {}x{}: {:?}", src.width, src.height, t.elapsed());

    let renderer = Renderer::new();
    let t = Instant::now();
    let color = primary_color(&src, &renderer.adjuster);
    println!("color {:?}: {:?}", color, t.elapsed());

    let formatted = format_date(date);
    let info = ConcertInfo {
        band_name: band,
        date: &formatted,
        venue,
    };

    for (name, (w, h)) in [("horiz", HORIZ_SIZE), ("vert", VERT_SIZE)] {
        let t = Instant::now();
        let stored = renderer.render(&src, &color, &info, w, h);
        println!("render {}: {:?}", name, t.elapsed());

        let img = PackedImage::parse(&stored).expect("stored image");
        let pixels: Vec<u8> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| img.get(x, y))
            .collect();

        let file = fs::File::create(format!("{}_{}.png", out, name)).unwrap();
        let mut enc = png::Encoder::new(BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette(PALETTE.concat());
        enc.write_header()
            .unwrap()
            .write_image_data(&pixels)
            .unwrap();
    }
}
