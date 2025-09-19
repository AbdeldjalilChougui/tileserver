use tileserver::pmtiles::*;
use tileserver::source::TileFormat;

#[test]
fn tile_ids_match_the_spec() {
    // Values from the PMTiles v3 spec / reference implementation tests.
    assert_eq!(zxy_to_id(0, 0, 0), 0);
    assert_eq!(zxy_to_id(1, 0, 0), 1);
    assert_eq!(zxy_to_id(1, 0, 1), 2);
    assert_eq!(zxy_to_id(1, 1, 1), 3);
    assert_eq!(zxy_to_id(1, 1, 0), 4);
    assert_eq!(zxy_to_id(2, 0, 0), 5);
    assert_eq!(zxy_to_id(3, 0, 0), 21);
    assert_eq!(zxy_to_id(3, 7, 0), 84);
    assert_eq!(zxy_to_id(20, 0, 0), 366503875925);
}

#[test]
fn tile_ids_round_trip_and_are_dense() {
    for z in 0..=6u8 {
        let n = 1u32 << z;
        let mut seen = std::collections::HashSet::new();
        for x in 0..n {
            for y in 0..n {
                let id = zxy_to_id(z, x, y);
                assert_eq!(id_to_zxy(id), (z, x, y));
                seen.insert(id);
            }
        }
        let first = zxy_to_id(z, 0, 0);
        assert!(seen.iter().all(|&id| id >= first && id < first + (n as u64 * n as u64)));
        assert_eq!(seen.len(), (n * n) as usize);
    }
    for &(z, x, y) in &[(14u8, 8000u32, 6000u32), (24, 12345678, 9876543), (31, (1 << 31) - 1, 0)] {
        assert_eq!(id_to_zxy(zxy_to_id(z, x, y)), (z, x, y));
    }
}

#[test]
fn directory_round_trip_and_find() {
    let entries = vec![
        Entry { tile_id: 0, offset: 0, length: 10, run_length: 1 },
        Entry { tile_id: 1, offset: 10, length: 5, run_length: 3 }, // contiguous: offset encoded as 0
        Entry { tile_id: 7, offset: 0, length: 10, run_length: 1 },  // deduplicated content
        Entry { tile_id: 100, offset: 500, length: 42, run_length: 0 }, // leaf pointer
    ];
    let enc = encode_directory(&entries);
    assert_eq!(decode_directory(&enc).unwrap(), entries);
    assert_eq!(find(&entries, 0).unwrap().tile_id, 0);
    assert_eq!(find(&entries, 3).unwrap().tile_id, 1); // inside the run 1..4
    assert!(find(&entries, 4).is_none()); // after the run
    assert_eq!(find(&entries, 7).unwrap().offset, 0);
    assert_eq!(find(&entries, 5000).unwrap().run_length, 0); // falls into the leaf
    assert!(find(&[], 3).is_none());
    assert!(decode_directory(&[5, 1]).is_err());
}

#[test]
fn header_round_trip() {
    let h = Header {
        root_offset: 127, root_length: 10, metadata_offset: 137, metadata_length: 20, leaf_offset: 157,
        leaf_length: 0, data_offset: 157, data_length: 99, addressed_tiles: 3, tile_entries: 2, tile_contents: 2,
        clustered: true, internal_compression: Compression::Gzip, tile_compression: Compression::None,
        tile_type: TileFormat::Png, min_zoom: 0, max_zoom: 5, bounds: [-8.6, 19.0, 11.9, 37.1], center_zoom: 4,
        center: [3.05, 36.75],
    };
    assert_eq!(Header::parse(&h.to_bytes()).unwrap(), h);
    let mut bad = h.to_bytes();
    bad[7] = 2;
    assert!(Header::parse(&bad).is_err());
    assert!(Header::parse(b"nope").is_err());
}

fn write_and_read(max_root: usize) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.pmtiles");
    let mut w = Writer::new(TileFormat::Png, Compression::None);
    w.max_root_entries = max_root;
    w.metadata = serde_json::json!({"name": "test"});
    for z in 0..=4u8 {
        for x in 0..(1u32 << z) {
            for y in 0..(1u32 << z) {
                // Many identical tiles at z4 (ocean), unique ones elsewhere.
                let body = if z == 4 && x > 3 { b"ocean".to_vec() } else { format!("{z}/{x}/{y}").into_bytes() };
                w.add(z, x, y, body);
            }
        }
    }
    w.write_to(&path).unwrap();
    let p = PmTiles::open(&path).unwrap();
    assert_eq!(p.metadata["name"], "test");
    assert_eq!((p.header.min_zoom, p.header.max_zoom), (0, 4));
    assert!(p.header.tile_contents < p.header.addressed_tiles, "dedup must kick in");
    for z in 0..=4u8 {
        for x in 0..(1u32 << z) {
            for y in 0..(1u32 << z) {
                let want = if z == 4 && x > 3 { b"ocean".to_vec() } else { format!("{z}/{x}/{y}").into_bytes() };
                assert_eq!(p.tile(z, x, y).unwrap(), Some(want), "{z}/{x}/{y}");
            }
        }
    }
    assert_eq!(p.tile(5, 0, 0).unwrap(), None);
    if max_root < 50 {
        assert!(p.header.leaf_length > 0, "expected leaf directories");
    }
}

#[test]
fn writer_reader_round_trip_root_only() {
    write_and_read(10_000);
}

#[test]
fn writer_reader_round_trip_with_leaves() {
    write_and_read(8);
}
