use std::{env, error::Error, time::Duration};
use voxio::Vox;

fn main() -> Result<(), Box<dyn Error>> {
    let path1 = env::args()
        .nth(1)
        .unwrap_or_else(|| "tests/test_suite/part1.mp3".to_string());
    let path2 = env::args()
        .nth(2)
        .unwrap_or_else(|| "tests/test_suite/part2.mp3".to_string());

    let (vox, events) = Vox::new().expect("Failed to create Vox instance");

    vox.play(&path1)?;
    vox.set_next(&path2)?;

    while let Some(event) = events.recv_active(Duration::from_millis(100)) {
        println!("{event:?}");
    }

    Ok(())
}
