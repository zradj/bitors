# <ins>BitTor</ins>rent for <ins>R</ins>u<ins>s</ins>t (bitors)

A Rust crate for parsing and creating BitTorrent metainfo files (`.torrent`).

`bitors` supports BitTorrent v1 ([BEP 3](https://www.bittorrent.org/beps/bep_0003.html)),
v2 ([BEP 52](https://www.bittorrent.org/beps/bep_0052.html)), and hybrid torrents. It can read
existing torrents without copying their data, build new ones from files on disk, and turn any
torrent into a [magnet link](https://en.wikipedia.org/wiki/Magnet_URI_scheme).

## Features

- **Zero-copy parsing.** [`Parser`](crate::Parser) borrows from the input buffer, and the parsed
  types ([`Bencode`](crate::bencode::Bencode), [`Torrent`](crate::Torrent), and friends) hold
  `Cow<'a, ...>` values tied to it. Call [`into_owned`](crate::torrent::IntoOwned::into_owned) to
  detach a torrent from its buffer.
- **v1, v2, and hybrid torrents.** The metainfo is modeled by
  [`TorrentMeta`](crate::torrent::TorrentMeta), so every version-specific field is only reachable in
  the variants where it exists.
- **A typestate builder.** [`TorrentBuilder`](crate::TorrentBuilder) will not let you build a torrent
  until at least one path has been supplied, so an empty torrent is a compile-time error.
- **Fast hashing.** Pieces are hashed in parallel with [`rayon`](https://docs.rs/rayon), files are
  memory-mapped, and v2 Merkle trees are computed alongside the v1 piece hashes when building a
  hybrid torrent. SHA-1 and SHA-256 come from the [RustCrypto](https://github.com/RustCrypto/hashes)
  crates, which use hardware acceleration where available.
- **Info hashes and magnet links.** Compute the v1 (SHA-1) and v2 (SHA-256) info hashes and
  generate magnet links with [`MagnetLink`](crate::magnet::MagnetLink).
- **Strict bencode.** The parser rejects leading zeros, negative zero, unsorted or duplicate
  dictionary keys, non-string keys, and nesting deeper than a configurable limit.

## Quick start

### Parsing a torrent

```no_run
use std::fs;

use bitors::parse_torrent;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let data = fs::read("my_torrent.torrent")?;
let torrent = parse_torrent(&data)?;

println!("name: {}", torrent.name());
println!("files: {}", torrent.file_count());
println!("size: {} bytes", torrent.total_size());

if let Some(v1) = torrent.info_hash_v1() {
    let hex: String = v1.iter().map(|b| format!("{b:02x}")).collect();
    println!("v1 info hash: {hex}");
}
# Ok(())
# }
```

`parse_torrent` returns a [`Torrent`](crate::Torrent) that borrows from `data`. If you need to keep
the torrent after `data` is dropped, convert it with
[`IntoOwned::into_owned`](crate::torrent::IntoOwned::into_owned).

### Creating a torrent

```no_run
use std::{fs::File, io::BufWriter};

use bitors::Torrent;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let torrent = Torrent::builder()
    .private(true)
    .comment("Created with bitors")
    .add_tracker("https://tracker.example.com/announce".parse()?)
    .add_path("my_folder")
    .build()?; // hybrid (v1 + v2) by default

let mut writer = BufWriter::new(File::create("my_folder.torrent")?);
torrent.to_bencode().encode_to_writer(&mut writer)?;
# Ok(())
# }
```

[`build`](crate::TorrentBuilder::build) produces a hybrid torrent. Use
[`build_v1`](crate::TorrentBuilder::build_v1) or
[`build_v2`](crate::TorrentBuilder::build_v2) when you need a single version. Fields you do not set
get sensible defaults; see the [`TorrentBuilder`](crate::TorrentBuilder) documentation for the full
list.

### Generating a magnet link

```no_run
use bitors::Torrent;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let torrent = Torrent::builder().add_path("my_file").build()?;

println!("{}", torrent.magnet_link());
// magnet:?xt=urn:btih:<v1 hash>&xt=urn:btmh:1220<v2 hash>&dn=my_file&xl=<size>...

// Some clients prefer the v1 hash in Base32.
println!("{}", torrent.magnet_link().v1_base32());
# Ok(())
# }
```

### Working with raw bencode

The [`bencode`](crate::bencode) module is usable on its own:

```
use bitors::Parser;

let bencode = Parser::new(b"d3:bar4:spam3:fooi42ee").parse().unwrap();
let dict = bencode.as_dict().unwrap();

assert_eq!(dict[&b"foo"[..]].as_int().unwrap(), 42);
assert_eq!(bencode.encode(), b"d3:bar4:spam3:fooi42ee");
```

## Crate layout

| Module                                        | Contents                                                                                                    |
| --------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| [`bencode`](crate::bencode)                   | The [`Bencode`](crate::bencode::Bencode) value type, the zero-copy [`Parser`](crate::Parser), and encoding. |
| [`torrent`](crate::torrent)                   | [`Torrent`](crate::Torrent) and the types that model the `info` dictionary, file lists, and file trees.     |
| [`torrent::builder`](crate::torrent::builder) | [`TorrentBuilder`](crate::TorrentBuilder), which hashes files and assembles new torrents.                   |
| [`magnet`](crate::magnet)                     | [`MagnetLink`](crate::magnet::MagnetLink) and [`InfoHashes`](crate::magnet::InfoHashes).                    |
| [`error`](crate::error)                       | The crate-wide [`Error`](crate::error::Error), which wraps the per-module error types.                      |

## Limitations

- The only supported torrent `encoding` is UTF-8.
- Bencode integers are signed 64-bit, so a single file larger than `i64::MAX` bytes (≈ 8 EiB) cannot be
  represented.
- Padding files are handled according to [BEP 47](https://www.bittorrent.org/beps/bep_0047.html)
  when building hybrid torrents, and are ignored when checking that the v1 and v2 file lists of a
  hybrid torrent agree.

## AI usage disclosure

This README and the module-level docs, as well as a few others, were written by an LLM and manually reviewed. The rest of the documentation and the entire codebase were written manually.

## Minimum supported Rust version

Rust 1.88 or newer (edition 2024).

## License

Licensed under either of the [MIT license](https://opensource.org/licenses/MIT) or the
[Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0) at your option.
