//! A crate for parsing and creating BitTorrent metainfo files (`.torrent`).
//!
//! `bitors` provides two main capabilities:
//!
//! 1. **Parsing** — read an existing `.torrent` file into a structured [`torrent::Torrent`].
//! 2. **Creation** — build a new `.torrent` from files on disk and serialize it back to bytes.
//!
//! Both capabilities sit on top of a zero-copy [Bencode] parser that can also be used
//! independently.
//!
//! [Bencode]: bencode
//!
//! # Crate structure
//!
//! | Module | Purpose |
//! |---|---|
//! | [`bencode`] | Zero-copy Bencode parser and encoder |
//! | [`torrent`] | `.torrent` metainfo types, parser, builder, and factory |
//! | [`error`] | Top-level error enum that aggregates sub-module errors |
//!
//! # Parsing a `.torrent` file
//!
//! The typical parsing pipeline has three steps:
//!
//! 1. Feed raw bytes into [`bencode::Parser`] to get a generic [`bencode::Bencode`] tree.
//! 2. Convert the tree into a typed [`torrent::Torrent`] with [`TryFrom`] / [`TryInto`].
//! 3. Use the resulting struct to inspect trackers, file lists, and piece hashes.
//!
//! ```no_run
//! use std::{fs, io::Read};
//! use bitors::{
//!     bencode::Parser,
//!     torrent::Torrent,
//! };
//!
//! let bytes = fs::read("ubuntu.torrent")?;
//! let mut parser = Parser::new(&bytes);
//! let bencode = parser.parse()?;
//! let torrent: Torrent<'_> = (&bencode).try_into()?;
//!
//! println!("Name:         {}", torrent.info.name);
//! println!("Piece length: {}", torrent.info.piece_length);
//! println!("Trackers:     {:?}", torrent.trackers());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Creating a `.torrent` file
//!
//! `bitors` offers two construction paths, suited to different use-cases.
//!
//! ## `TorrentFactory` — build from files on disk
//!
//! [`torrent::factory::TorrentFactory`] reads source files, computes SHA-1 piece hashes,
//! and assembles the complete `info` dictionary for you. It uses the *typestate* pattern to
//! make it a **compile-time error** to call [`build`] before any source files have been
//! provided.
//!
//! [`build`]: torrent::factory::TorrentFactory::build
//!
//! ```no_run
//! use std::num::NonZeroU64;
//! use url::Url;
//! use bitors::{
//!     bencode::Bencode,
//!     torrent::factory::TorrentFactory,
//! };
//!
//! // Single-file torrent
//! let torrent = TorrentFactory::new()
//!     .piece_length(NonZeroU64::new(512 * 1024).unwrap())
//!     .add_announce(Url::parse("udp://tracker.example.com:6969/announce")?)
//!     .add_file("path/to/file.iso")?
//!     .build()?;
//!
//! // Serialize to a .torrent file
//! let mut out = std::fs::File::create("file.torrent")?;
//! Bencode::from(&torrent).encode_to_writer(&mut out)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! For multi-file (directory) torrents, use [`TorrentFactory::from_directory`]:
//!
//! ```no_run
//! use bitors::torrent::factory::TorrentFactory;
//!
//! let torrent = TorrentFactory::from_directory("path/to/my-album/")?
//!     .private()
//!     .build()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`TorrentFactory::from_directory`]: torrent::factory::TorrentFactory::from_directory
//!
//! ## `TorrentBuilder` — assemble from an existing `Info`
//!
//! [`torrent::builder::TorrentBuilder`] is a simpler option when you already have a
//! fully constructed [`torrent::InfoBuf`] (for example, after parsing an `info` dictionary
//! or computing hashes yourself). It validates that the resulting torrent has at least one
//! announce URL (unless it is marked private) and then assembles the top-level [`torrent::Torrent`].
//!
//! ```no_run
//! use bitors::torrent::{Torrent, InfoBuf};
//!
//! # fn get_info() -> InfoBuf { todo!() }
//! let info: InfoBuf = get_info();
//! let torrent = Torrent::builder(info)
//!     .announce("http://tracker.example.com/announce".parse()?)
//!     .comment("My release")
//!     .build()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Zero-copy design and lifetimes
//!
//! [`bencode::Parser`] borrows directly from the source byte slice: all
//! [`Bencode::Bytes`](bencode::Bencode::Bytes) values and dictionary keys point into the
//! original buffer without copying. The parsed types in the [`torrent`] module mirror this
//! design — string fields use [`std::borrow::Cow`] and the `'a` lifetime parameter ties them
//! back to the source buffer.
//!
//! When you need a value that outlives the source buffer, call `into_owned()` on any
//! borrowing type to get its `'static` alias:
//!
//! | Borrowing type | Owned alias |
//! |---|---|
//! | `Torrent<'a>` | [`torrent::TorrentBuf`] |
//! | `Info<'a>` | [`torrent::InfoBuf`] |
//! | `FileMode<'a>` | [`torrent::FileModeBuf`] |
//! | `FileInfo<'a>` | [`torrent::FileInfoBuf`] |
//!
//! # Round-trip fidelity
//!
//! Any `Torrent` (whether parsed or constructed) can be converted back to a `Bencode` value
//! via the [`From`] implementations in [`bencode`], and then serialized with
//! [`encode_to_writer`](bencode::Bencode::encode_to_writer). Fields that are `None` are
//! omitted from the output, matching the BitTorrent metainfo specification.
//!
//! # Feature flags
//!
//! This crate has no optional feature flags. All functionality is always available.
//!
//! # AI usage disclosure
//!
//! The source code in this crate was written manually, with recommendations
//! from large language models (LLMs) consulted during development.  All
//! documentation — including this crate-level doc, module docs, and item-level
//! doc comments — was written in full by LLMs and subsequently reviewed
//! manually for accuracy.

#![warn(clippy::pedantic)]

pub mod bencode;
pub mod error;
pub mod torrent;
