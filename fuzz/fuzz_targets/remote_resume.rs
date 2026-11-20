//! Fuzz a resume over a hostile upload listing (ROADMAP 9.3.4; SPEC §9.4, ADR-0050 item 4): a shared bucket may hold
//! any object under `blobs/uploads/<upload_id>/` (any index up to `u32::MAX`, any length, foreign names), and
//! `RemoteBlobStore::resume` reads the listing's indices and lengths and the objects' bytes. Arbitrary objects never
//! panic, and what the resume keeps is exactly the run of full chunks from chunk 0 as stored: each kept chunk is the
//! object at its index, of the token's chunk size, with its CRC32C. `skip_kept` accepts those bytes and rejects them
//! one byte short, and `restage` copies the run to a new upload byte for byte.
//!
//! Input: byte 0 the listing order (even: reversed, odd: duplicated), byte 1 the chunk size (1..=16), byte 2 the
//! parallelism (1..=4), then records: a selector byte (bit 7: a foreign name from the next byte's length of text;
//! else bit 6: a 4-byte little-endian index, or the low bits as the index), a length byte (modulo twice the chunk
//! size plus one), and the object's bytes are derived from its index. The storage lists out of key order or twice,
//! which a resume must not rely on (SPEC §9.4).
#![no_main]

use core::num::NonZeroUsize;
use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use persia_blob::remote::{RemoteKey, ResumeToken};
use persia_blob::testing::{ListOrder, MisorderedList};
use persia_blob::{BlobStore, ChunkSize, Error, RemoteBlobStore, UploadId};
use persia_format::checksum::crc32c;
use persia_storage::{Key, MemoryStorage, Storage};

const UPLOAD: u128 = 0x5045_5253_4941;

fn object_bytes(index: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (index as u8).wrapping_mul(31) ^ (i as u8))
        .collect()
}

fuzz_target!(|data: &[u8]| {
    let [order_b, cs, par, rest @ ..] = data else {
        return;
    };
    let chunk = u32::from(cs % 16) + 1;
    let Ok(chunk_size) = ChunkSize::from_encoding(chunk) else {
        return;
    };
    let parallelism = NonZeroUsize::new(usize::from(par % 4) + 1).unwrap_or(NonZeroUsize::MIN);
    let order = if order_b % 2 == 0 {
        ListOrder::Reversed
    } else {
        ListOrder::Duplicated
    };
    let store = RemoteBlobStore::new(MisorderedList {
        inner: MemoryStorage::new(),
        order,
    })
    .with_parallelism(parallelism);
    let upload_id = UploadId::new(UPLOAD);
    let prefix = format!("blobs/uploads/{upload_id}/");

    // The objects as stored, by chunk index (a later record replaces an earlier one at the same index).
    let mut chunks: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut input = rest;
    while let [sel, len, tail @ ..] = input {
        input = tail;
        if sel & 0x80 != 0 {
            // A foreign name under the prefix: never a chunk.
            let n = usize::from(*len % 24).min(input.len());
            let (name, tail) = input.split_at(n);
            input = tail;
            let Ok(name) = core::str::from_utf8(name) else {
                continue;
            };
            let Ok(key) = Key::new(format!("{prefix}{name}")) else {
                continue;
            };
            if let Some(RemoteKey::Chunk {
                upload_id: u,
                index,
            }) = RemoteKey::parse(&key)
            {
                if u == upload_id {
                    // It spells a chunk key after all (a canonical one): it is that chunk.
                    let bytes = object_bytes(index, 0);
                    chunks.insert(index, bytes.clone());
                    store.storage().put(&key, bytes.into()).expect("memory put");
                }
                continue;
            }
            store
                .storage()
                .put(&key, b"foreign".to_vec().into())
                .expect("memory put");
            continue;
        }
        let index = if sel & 0x40 != 0 {
            let Some((raw, tail)) = input.split_first_chunk::<4>() else {
                break;
            };
            input = tail;
            u32::from_le_bytes(*raw)
        } else {
            u32::from(sel & 0x3f)
        };
        let len = usize::from(*len) % (2 * chunk as usize + 1);
        let bytes = object_bytes(index, len);
        let key = RemoteKey::Chunk { upload_id, index }
            .key()
            .expect("a chunk key");
        store
            .storage()
            .put(&key, bytes.clone().into())
            .expect("memory put");
        chunks.insert(index, bytes);
    }

    let token = ResumeToken {
        upload_id,
        chunk_size,
    };
    let resumed = store.resume(token).expect("a memory listing does not fail");
    // The run as stored: chunks 0, 1, … each of the chunk size.
    let run: Vec<&Vec<u8>> = (0..)
        .map_while(|i| chunks.get(&i).filter(|c| c.len() == chunk as usize))
        .collect();
    assert_eq!(resumed.next_index() as usize, run.len(), "the kept run");
    let crcs: Vec<u32> = run.iter().map(|c| crc32c(c)).collect();
    assert_eq!(resumed.chunk_crcs(), &crcs[..]);
    assert_eq!(resumed.offset(), run.len() as u64 * u64::from(chunk));
    let listed: Vec<u32> = chunks.keys().copied().collect();
    assert_eq!(
        resumed.listed(),
        &listed[..],
        "every chunk listed, in order"
    );

    let kept: Vec<u8> = run.iter().flat_map(|c| c.iter().copied()).collect();
    resumed.skip_kept(&mut &kept[..]).expect("the kept bytes");
    if let Some(short) = kept.len().checked_sub(1) {
        assert!(matches!(
            resumed.skip_kept(&mut &kept[..short]),
            Err(Error::InvalidResume { .. })
        ));
    }

    let to = UploadId::new(UPLOAD + 1);
    store.restage(token, &crcs, to).expect("the run restages");
    for (i, c) in (0..).zip(&run) {
        let key = RemoteKey::Chunk {
            upload_id: to,
            index: i,
        }
        .key()
        .expect("a chunk key");
        assert_eq!(&store.storage().get(&key).expect("restaged")[..], &c[..]);
    }
    assert_eq!(
        BlobStore::resume(
            &store,
            ResumeToken {
                upload_id: to,
                ..token
            }
        )
        .expect("resume")
        .next_index() as usize,
        run.len()
    );
});
