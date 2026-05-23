# bitors
[![crates.io](https://img.shields.io/crates/v/bitors.svg)](https://crates.io/crates/bitors)

A Rust library for parsing and creating BitTorrent metainfo files (`.torrent`).

`bitors` gives you two things:

1. **Parsing** — read an existing `.torrent` file into a typed, zero-copy Rust struct.
2. **Creation** — build a new `.torrent` from files on disk and serialize it to bytes.

Both are built on top of a standalone zero-copy [Bencode] parser that you can also
use independently.

[Bencode]: https://www.bittorrent.org/beps/bep_0003.html#bencoding

---

## Installation

Add `bitors` to your `Cargo.toml`:

```toml
[dependencies]
bitors = "1.2.0"
```

---

## Quick start

### Parsing a `.torrent` file

```rust
use std::fs;
use bitors::{bencode::Parser, torrent::Torrent};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read("ubuntu.torrent")?;

    let mut parser = Parser::new(&bytes);
    let bencode = parser.parse()?;
    let torrent: Torrent<'_> = (&bencode).try_into()?;

    println!("Name:         {}", torrent.info.name);
    println!("Piece length: {} bytes", torrent.info.piece_length);
    println!("Pieces:       {}", torrent.info.pieces.len());
    println!("Trackers:     {:?}", torrent.trackers());
    println!("Total size:   {} bytes", torrent.total_size());
    println!("File count:   {}", torrent.file_count());

    let hash = torrent.info_hash();
    println!("Info hash:    {}", hash.map(|b| format!("{b:02x}")).join(""));

    Ok(())
}
```

`Parser` borrows directly from your byte slice — string and byte fields in the
returned `Torrent<'_>` point into `bytes` without copying. Call `.into_owned()` if
you need a value that outlives the buffer.

---

### Creating a `.torrent` from a file

```rust
use std::num::NonZeroU64;
use bitors::{bencode::Bencode, torrent::factory::TorrentFactory};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let torrent = TorrentFactory::new()
        .piece_length(NonZeroU64::new(512 * 1024).unwrap())
        .add_announce("udp://tracker.opentrackr.org:1337/announce".parse()?)
        .add_file("path/to/file.iso")?
        .build()?;

    let mut out = std::fs::File::create("file.torrent")?;
    Bencode::from(&torrent).encode_to_writer(&mut out)?;

    Ok(())
}
```

`TorrentFactory` reads the file, computes SHA-1 piece hashes, and fills in the
`info` dictionary. The typestate pattern means you get a **compile error** if you
call `.build()` before supplying any files.

---

### Creating a `.torrent` from a directory

```rust
use bitors::torrent::factory::TorrentFactory;

let torrent = TorrentFactory::from_directory("path/to/my-album/")?
    .add_announce("udp://tracker.opentrackr.org:1337/announce".parse()?)
    .private()
    .build()?;
```

Files are walked recursively and sorted lexicographically so that piece hashes
are reproducible across runs.

---

### Using the builder directly

When you already have an `InfoBuf` (e.g. from parsing an existing torrent), use
`TorrentBuilder` to attach top-level metadata without re-hashing anything:

```rust
use bitors::torrent::{Torrent, InfoBuf};

# fn make_info() -> InfoBuf { todo!() }
let torrent = Torrent::builder(make_info())
    .announce("udp://tracker.opentrackr.org:1337/announce".parse()?)
    .comment("re-seeded by archiver")
    .created_by("my-tool/1.0")
    .build()?;
```

---

## Crate structure

| Module | Contents |
|---|---|
| `bitors::bencode` | `Bencode` enum, `Parser`, encoding methods |
| `bitors::torrent` | `Torrent`, `Info`, `FileMode`, `FileInfo`, owned type aliases |
| `bitors::torrent::builder` | `TorrentBuilder` — wrap an existing `InfoBuf` |
| `bitors::torrent::factory` | `TorrentFactory` — build from files on disk |
| `bitors::error` | Top-level `Error` enum aggregating sub-module errors |

---

## Info hash

`Torrent::info_hash` (and its delegate `Info::info_hash`) return the 20-byte SHA-1 hash
of the Bencoded `info` dictionary — the canonical torrent identifier exchanged with
trackers and embedded in magnet links:

```rust
let hash = torrent.info_hash();
println!("{}", hash.map(|b| format!("{b:02x}")).join(""));
```

---

## Zero-copy design and lifetimes

`Parser` borrows from the source buffer. Every type produced by parsing carries a
lifetime parameter `'a` tying it back to that buffer. When you need a
self-contained, heap-allocated value, call `into_owned()`:

| Borrowing type | Owned alias |
|---|---|
| `Torrent<'a>` | `TorrentBuf` |
| `Info<'a>` | `InfoBuf` |
| `FileMode<'a>` | `FileModeBuf` |
| `FileInfo<'a>` | `FileInfoBuf` |

Values produced by `TorrentFactory` or `TorrentBuilder` are always owned
(`'static`) and never require `into_owned()`.

---

## Tracker tiers (BEP 12)

Both `TorrentFactory` and `TorrentBuilder` support the multi-tracker extension
([BEP 12]). Build a tiered list with `next_announce_tier()`:

```rust
use bitors::torrent::factory::TorrentFactory;

let factory = TorrentFactory::new()
    // Tier 0 — primary trackers (tried first, in random order)
    .add_announce("udp://tracker.opentrackr.org:1337/announce".parse()?)
    .add_announce("udp://tracker.torrent.eu.org:451/announce".parse()?)
    .next_announce_tier()
    // Tier 1 — fallback trackers
    .add_announce("udp://open.stealth.si:80/announce".parse()?);
```

The first URL of the first tier is also written to the top-level `announce` key
for compatibility with older clients.

[BEP 12]: https://www.bittorrent.org/beps/bep_0012.html

---

## Web seeds (BEP 19)

The `url_list` field on `Torrent` holds a list of HTTP/HTTPS URLs from which clients
may download content when peers are scarce. Both `TorrentFactory` and `TorrentBuilder`
expose builder methods for populating this list:

```rust
use bitors::torrent::factory::TorrentFactory;

let torrent = TorrentFactory::from_file("file.iso")?
    .add_url("https://mirror.example.com/file.iso".parse()?)
    .add_announce("udp://tracker.example.com:6969/announce".parse()?)
    .build()?;
```

[BEP 19]: https://www.bittorrent.org/beps/bep_0019.html

---

## Round-trip fidelity

Any `Torrent` — whether parsed or constructed — can be serialized back to bytes:

```rust
use bitors::bencode::Bencode;

// torrent: TorrentBuf (or any Torrent<'_>)
let bencode = Bencode::from(&torrent);

// Write to a file
let mut f = std::fs::File::create("output.torrent")?;
bencode.encode_to_writer(&mut f)?;

// Or get a Vec<u8>
let bytes: Vec<u8> = bencode.encode();
```

`None` fields are omitted from the output dictionary, matching the BitTorrent
metainfo specification.

---

## AI usage disclosure

The source code in this crate was written manually, with recommendations from
large language models (LLMs) consulted during development. All documentation —
including the crate-level doc, module docs, item-level doc comments, and this
README — was written in full by LLMs and subsequently reviewed manually for
accuracy.

---

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
