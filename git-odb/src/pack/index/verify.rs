use crate::pack::{cache, DecodeEntryResult};
use crate::{pack, pack::index};
use git_features::progress::{self, Progress};
use git_object::SHA1_SIZE;
use quick_error::quick_error;
use smallvec::alloc::collections::BTreeMap;
use std::time::Instant;

quick_error! {
    #[derive(Debug)]
    pub enum ChecksumError {
        Mismatch { expected: git_object::Id, actual: git_object::Id } {
            display("index checksum mismatch: expected {}, got {}", expected, actual)
        }
        PackChecksum(err: pack::ChecksumError) {
            display("The pack of this index file failed to verify its checksums")
            from()
            cause(err)
        }
        PackDecode(err: pack::Error, id: git_object::Id, offset: u64) {
            display("Object {} at offset {} could not be decoded", id, offset)
            cause(err)
        }
        PackMismatch { expected: git_object::Id, actual: git_object::Id } {
            display("The packfiles checksum didn't match the index file checksum: expected {}, got {}", expected, actual)
        }
        PackObjectMismatch { expected: git_object::Id, actual: git_object::Id, offset: u64, kind: git_object::Kind} {
            display("The SHA1 of {} object at offset {} didn't match the checksum in the index file: expected {}, got {}", kind, offset, expected, actual)
        }
        Crc32Mismatch { expected: u32, actual: u32, offset: u64, kind: git_object::Kind} {
            display("The CRC32 of {} object at offset {} didn't match the checksum in the index file: expected {}, got {}", kind, offset, expected, actual)
        }
    }
}

struct TimeThroughput {
    then: Instant,
    byte_size: usize,
}

impl TimeThroughput {
    pub fn new(byte_size: usize) -> TimeThroughput {
        TimeThroughput {
            then: Instant::now(),
            byte_size,
        }
    }
}

impl Into<String> for TimeThroughput {
    fn into(self) -> String {
        let time_taken = std::time::Instant::now().duration_since(self.then).as_secs_f32();
        format!(
            "finished in {:.2}s at {}/s",
            time_taken,
            bytesize::ByteSize((self.byte_size as f32 / time_taken) as u64)
        )
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Clone)]
#[cfg_attr(feature = "serde1", derive(serde::Serialize, serde::Deserialize))]
pub struct PackFileChecksumResult {
    pub average: DecodeEntryResult,
    pub objects_per_chain_length: BTreeMap<u32, u32>,
    /// The amount of bytes in all compressed streams, one per entry
    pub total_compressed_entries_size: u64,
    /// The amount of bytes in all decompressed streams, one per entry
    pub total_decompressed_entries_size: u64,
    /// The amount of bytes occupied by all undeltified, decompressed objects
    pub total_object_size: u64,
    /// The amount of bytes occupied by the pack itself, in bytes
    pub pack_size: u64,
}

/// Methods to verify and validate the content of the index file
impl index::File {
    pub fn checksum_of_index(&self) -> git_object::Id {
        git_object::Id::from_20_bytes(&self.data[self.data.len() - SHA1_SIZE..])
    }

    pub fn checksum_of_pack(&self) -> git_object::Id {
        let from = self.data.len() - SHA1_SIZE * 2;
        git_object::Id::from_20_bytes(&self.data[from..from + SHA1_SIZE])
    }

    /// If `pack` is provided, it is expected (and validated to be) the pack belonging to this index.
    /// It will be used to validate internal integrity of the pack before checking each objects integrity
    /// is indeed as advertised via its SHA1 as stored in this index, as well as the CRC32 hash.
    pub fn verify_checksum_of_index<P, C>(
        &self,
        pack: Option<&pack::File>,
        progress: Option<P>,
        make_cache: impl Fn() -> C + Send + Sync,
    ) -> Result<(git_object::Id, Option<PackFileChecksumResult>), ChecksumError>
    where
        P: Progress,
        <P as Progress>::SubProgress: Send,
        C: cache::DecodeEntry,
    {
        use crate::pack::ResolvedBase;
        use git_features::parallel::{self, in_parallel_if};

        let mut root = progress::DoOrDiscard::from(progress);
        let mut progress = root.add_child("Sha1 of index");

        let mut verify_self = move || {
            let throughput = TimeThroughput::new(self.data.len());
            let mut hasher = git_features::hash::Sha1::default();
            hasher.update(&self.data[..self.data.len() - SHA1_SIZE]);
            let actual = git_object::Id(hasher.digest());
            progress.done(throughput);

            let expected = self.checksum_of_index();
            if actual == expected {
                Ok(actual)
            } else {
                Err(ChecksumError::Mismatch { actual, expected })
            }
        };
        match pack {
            None => verify_self().map(|id| (id, None)),
            Some(pack) => {
                if self.checksum_of_pack() != pack.checksum() {
                    return Err(ChecksumError::PackMismatch {
                        actual: pack.checksum(),
                        expected: self.checksum_of_pack(),
                    });
                }
                let mut progress = root.add_child(format!("Sha1 of pack at '{}'", pack.path().display()));
                let (pack_res, id) = parallel::join(
                    move || {
                        let throughput = TimeThroughput::new(pack.data_len());
                        let res = pack.verify_checksum();
                        progress.done(throughput);
                        res
                    },
                    verify_self,
                );
                pack_res?;
                let id = id?;

                let index_entries = {
                    let mut v: Vec<_> = self.iter().collect();
                    v.sort_by_key(|e| e.pack_offset);
                    v
                };

                fn add_decode_result(lhs: &mut DecodeEntryResult, rhs: DecodeEntryResult) {
                    lhs.num_deltas += rhs.num_deltas;
                    lhs.decompressed_size += rhs.decompressed_size;
                    lhs.compressed_size += rhs.compressed_size;
                    lhs.object_size += rhs.object_size;
                }

                fn div_decode_result(lhs: &mut DecodeEntryResult, div: usize) {
                    lhs.num_deltas = (lhs.num_deltas as f32 / div as f32) as u32;
                    lhs.decompressed_size /= div as u64;
                    lhs.compressed_size /= div;
                    lhs.object_size /= div as u64;
                }

                struct Reducer<'a, P> {
                    progress: &'a std::sync::Mutex<P>,
                    then: Instant,
                    entries_seen: u32,
                    chunks_seen: usize,
                    stats: PackFileChecksumResult,
                }

                impl<'a, P> parallel::Reducer for Reducer<'a, P>
                where
                    P: Progress,
                {
                    type Input = Result<Vec<DecodeEntryResult>, ChecksumError>;
                    type Output = PackFileChecksumResult;
                    type Error = ChecksumError;

                    fn feed(&mut self, input: Self::Input) -> Result<(), Self::Error> {
                        let chunk_stats: Vec<_> = input?;
                        let num_entries_in_chunk = chunk_stats.len();
                        self.entries_seen += num_entries_in_chunk as u32;
                        self.chunks_seen += 1;

                        let mut chunk_average = chunk_stats.into_iter().fold(
                            DecodeEntryResult::default_from_kind(git_object::Kind::Tree),
                            |mut average, stats| {
                                *self.stats.objects_per_chain_length.entry(stats.num_deltas).or_insert(0) += 1;
                                self.stats.total_decompressed_entries_size += stats.decompressed_size;
                                self.stats.total_compressed_entries_size += stats.compressed_size as u64;
                                self.stats.total_object_size += stats.object_size as u64;
                                add_decode_result(&mut average, stats);
                                average
                            },
                        );
                        div_decode_result(&mut chunk_average, num_entries_in_chunk);
                        add_decode_result(&mut self.stats.average, chunk_average);

                        self.progress.lock().unwrap().set(self.entries_seen);
                        Ok(())
                    }

                    fn finalize(mut self) -> Result<Self::Output, Self::Error> {
                        self.progress.lock().unwrap().done("finished");
                        div_decode_result(&mut self.stats.average, self.chunks_seen);
                        let elapsed_s = Instant::now().duration_since(self.then).as_secs_f32();
                        self.progress.lock().unwrap().info(format!(
                            "Reduced {} objects in {:.2}s ({} objects/s)",
                            self.entries_seen,
                            elapsed_s,
                            (self.entries_seen as f32 / elapsed_s) as u32
                        ));
                        Ok(self.stats)
                    }
                }

                const CHUNK_SIZE: usize = 1000;
                let there_are_enough_entries_to_process = || index_entries.len() > CHUNK_SIZE * 2;
                let input_chunks = index_entries
                    .chunks(CHUNK_SIZE.max(index_entries.len() / CHUNK_SIZE))
                    .into_iter();
                let reduce_progress = std::sync::Mutex::new(root.add_child("reduce"));
                reduce_progress
                    .lock()
                    .unwrap()
                    .init(Some(self.num_objects()), Some("objects"));
                let state_per_thread = |index| {
                    (
                        make_cache(),
                        Vec::with_capacity(2048),
                        reduce_progress.lock().unwrap().add_child(format!("thread {}", index)),
                    )
                };

                let stats = in_parallel_if(
                    there_are_enough_entries_to_process,
                    input_chunks,
                    state_per_thread,
                    |entries: &[index::Entry],
                     (cache, buf, progress)|
                     -> Result<Vec<DecodeEntryResult>, ChecksumError> {
                        progress.init(Some(entries.len() as u32), Some("entries"));
                        let mut stats = Vec::with_capacity(entries.len());
                        for (idx, index_entry) in entries.iter().enumerate() {
                            let pack_entry = pack.entry(index_entry.pack_offset);
                            let pack_entry_data_offset = pack_entry.data_offset;
                            let entry_stats = pack
                                .decode_entry(
                                    pack_entry,
                                    buf,
                                    |id, _| {
                                        self.lookup_index(&id).map(|index| {
                                            ResolvedBase::InPack(pack.entry(self.pack_offset_at_index(index)))
                                        })
                                    },
                                    cache,
                                )
                                .map_err(|e| ChecksumError::PackDecode(e, index_entry.oid, index_entry.pack_offset))?;
                            let object_kind = entry_stats.kind;
                            let consumed_input = entry_stats.compressed_size;
                            stats.push(entry_stats);

                            let mut header_buf = [0u8; 64];
                            let header_size =
                                crate::loose::db::serde::write_header(object_kind, buf.len(), &mut header_buf[..])
                                    .expect("header buffer to be big enough");
                            let mut hasher = git_features::hash::Sha1::default();
                            hasher.update(&header_buf[..header_size]);
                            hasher.update(buf.as_slice());

                            let actual_oid = git_object::Id(hasher.digest());
                            if actual_oid != index_entry.oid {
                                return Err(ChecksumError::PackObjectMismatch {
                                    actual: actual_oid,
                                    expected: index_entry.oid.clone(),
                                    offset: index_entry.pack_offset,
                                    kind: object_kind,
                                });
                            }
                            if let Some(desired_crc32) = index_entry.crc32 {
                                let actual_crc32 = pack.entry_crc32(
                                    index_entry.pack_offset,
                                    (pack_entry_data_offset - index_entry.pack_offset) as usize + consumed_input,
                                );
                                if actual_crc32 != desired_crc32 {
                                    return Err(ChecksumError::Crc32Mismatch {
                                        actual: actual_crc32,
                                        expected: desired_crc32,
                                        offset: index_entry.pack_offset,
                                        kind: object_kind,
                                    });
                                }
                            }
                            progress.set(idx as u32);
                        }
                        Ok(stats)
                    },
                    Reducer {
                        progress: &reduce_progress,
                        then: Instant::now(),
                        entries_seen: 0,
                        chunks_seen: 0,
                        stats: PackFileChecksumResult {
                            average: DecodeEntryResult::default_from_kind(git_object::Kind::Tree),
                            objects_per_chain_length: Default::default(),
                            total_compressed_entries_size: 0,
                            total_decompressed_entries_size: 0,
                            total_object_size: 0,
                            pack_size: pack.data_len() as u64,
                        },
                    },
                )?;

                Ok((id, Some(stats)))
            }
        }
    }
}