use std::io::{self, Read, Write};

use common::{BinarySerializable, DeserializeFrom};
use tantivy_bitpacker::{compute_num_bits, BitPacker, BitUnpacker};

use crate::{FastFieldCodecReader, FastFieldCodecSerializer, FastFieldDataAccess, FastFieldStats};

const BLOCK_SIZE: u64 = 128;

#[derive(Clone)]
pub struct FORFastFieldReader {
    num_vals: u64,
    min_value: u64,
    max_value: u64,
    block_readers: Vec<BlockReader>,
}

#[derive(Clone, Debug, Default)]
struct BlockMetadata {
    min: u64,
    num_bits: u8,
}

#[derive(Clone, Debug, Default)]
struct BlockReader {
    metadata: BlockMetadata,
    start_offset: u64,
    bit_unpacker: BitUnpacker,
}

impl BlockReader {
    fn new(metadata: BlockMetadata, start_offset: u64) -> Self {
        Self {
            bit_unpacker: BitUnpacker::new(metadata.num_bits),
            metadata,
            start_offset,
        }
    }

    #[inline]
    fn get_u64(&self, block_pos: u64, data: &[u8]) -> u64 {
        let diff = self
            .bit_unpacker
            .get(block_pos, &data[self.start_offset as usize..]);
        self.metadata.min + diff
    }
}

impl BinarySerializable for BlockMetadata {
    fn serialize<W: Write>(&self, write: &mut W) -> io::Result<()> {
        self.min.serialize(write)?;
        self.num_bits.serialize(write)?;
        Ok(())
    }

    fn deserialize<R: Read>(reader: &mut R) -> io::Result<Self> {
        let min = u64::deserialize(reader)?;
        let num_bits = u8::deserialize(reader)?;
        Ok(Self { min, num_bits })
    }
}

#[derive(Clone, Debug)]
pub struct FORFooter {
    pub num_vals: u64,
    pub min_value: u64,
    pub max_value: u64,
    block_metadatas: Vec<BlockMetadata>,
}

impl BinarySerializable for FORFooter {
    fn serialize<W: Write>(&self, write: &mut W) -> io::Result<()> {
        let mut out = vec![];
        self.num_vals.serialize(&mut out)?;
        self.min_value.serialize(&mut out)?;
        self.max_value.serialize(&mut out)?;
        self.block_metadatas.serialize(&mut out)?;
        write.write_all(&out)?;
        (out.len() as u32).serialize(write)?;
        Ok(())
    }

    fn deserialize<R: Read>(reader: &mut R) -> io::Result<Self> {
        let footer = Self {
            num_vals: u64::deserialize(reader)?,
            min_value: u64::deserialize(reader)?,
            max_value: u64::deserialize(reader)?,
            block_metadatas: Vec::<BlockMetadata>::deserialize(reader)?,
        };
        Ok(footer)
    }
}

impl FastFieldCodecReader for FORFastFieldReader {
    /// Opens a fast field given a file.
    fn open_from_bytes(bytes: &[u8]) -> io::Result<Self> {
        let footer_len: u32 = (&bytes[bytes.len() - 4..]).deserialize()?;
        let (_, mut footer) = bytes.split_at(bytes.len() - (4 + footer_len) as usize);
        let footer = FORFooter::deserialize(&mut footer)?;
        let mut block_readers = Vec::with_capacity(footer.block_metadatas.len());
        let mut current_data_offset = 0;
        for block_metadata in footer.block_metadatas {
            let num_bits = block_metadata.num_bits;
            block_readers.push(BlockReader::new(block_metadata, current_data_offset));
            current_data_offset += num_bits as u64 * BLOCK_SIZE / 8;
        }
        Ok(Self {
            num_vals: footer.num_vals,
            min_value: footer.min_value,
            max_value: footer.max_value,
            block_readers,
        })
    }

    #[inline]
    fn get_u64(&self, idx: u64, data: &[u8]) -> u64 {
        let block_idx = (idx / BLOCK_SIZE) as usize;
        let block_pos = idx - (block_idx as u64) * BLOCK_SIZE;
        let block_reader = &self.block_readers[block_idx];
        block_reader.get_u64(block_pos, data)
    }

    #[inline]
    fn min_value(&self) -> u64 {
        self.min_value
    }
    #[inline]
    fn max_value(&self) -> u64 {
        self.max_value
    }
}

/// Same as LinearInterpolFastFieldSerializer, but working on chunks of CHUNK_SIZE elements.
pub struct FORFastFieldSerializer {}

impl FastFieldCodecSerializer for FORFastFieldSerializer {
    const NAME: &'static str = "FOR";
    const ID: u8 = 5;
    /// Creates a new fast field serializer.
    fn serialize(
        write: &mut impl Write,
        _: &impl FastFieldDataAccess,
        stats: FastFieldStats,
        data_iter: impl Iterator<Item = u64>,
        _data_iter1: impl Iterator<Item = u64>,
    ) -> io::Result<()> {
        let data = data_iter.collect::<Vec<_>>();
        let mut bit_packer = BitPacker::new();
        let mut block_metadatas = Vec::new();
        for data_pos in (0..data.len() as u64).step_by(BLOCK_SIZE as usize) {
            let block_num_vals = BLOCK_SIZE.min(data.len() as u64 - data_pos) as usize;
            let block_values = &data[data_pos as usize..data_pos as usize + block_num_vals];
            let mut min = block_values[0];
            let mut max = block_values[0];
            for &current_value in block_values[1..].iter() {
                min = min.min(current_value);
                max = max.max(current_value);
            }
            let num_bits = compute_num_bits(max - min);
            for current_value in block_values.iter() {
                bit_packer.write(current_value - min, num_bits, write)?;
            }
            bit_packer.flush(write)?;
            block_metadatas.push(BlockMetadata { min, num_bits });
        }
        bit_packer.close(write)?;

        let footer = FORFooter {
            num_vals: stats.num_vals,
            min_value: stats.min_value,
            max_value: stats.max_value,
            block_metadatas,
        };
        footer.serialize(write)?;
        Ok(())
    }

    fn is_applicable(
        _fastfield_accessor: &impl FastFieldDataAccess,
        stats: FastFieldStats,
    ) -> bool {
        stats.num_vals > BLOCK_SIZE
    }

    /// Estimate compression ratio by compute the ratio of the first block.
    fn estimate_compression_ratio(
        fastfield_accessor: &impl FastFieldDataAccess,
        stats: FastFieldStats,
    ) -> f32 {
        let last_elem_in_first_chunk = BLOCK_SIZE.min(stats.num_vals);
        let max_distance = (0..last_elem_in_first_chunk)
            .into_iter()
            .map(|pos| {
                let actual_value = fastfield_accessor.get_val(pos as u64);
                actual_value - stats.min_value
            })
            .max()
            .unwrap();

        // Estimate one block and multiply by a magic number 3 to select this codec
        // when we are almost sure that this is relevant.
        let relative_max_value = max_distance as f32 * 3.0;

        let num_bits = compute_num_bits(relative_max_value as u64) as u64 * stats.num_vals as u64
            // function metadata per block
            + 9 * (stats.num_vals / BLOCK_SIZE);
        let num_bits_uncompressed = 64 * stats.num_vals;
        num_bits as f32 / num_bits_uncompressed as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::get_codec_test_data_sets;

    fn create_and_validate(data: &[u64], name: &str) -> (f32, f32) {
        crate::tests::create_and_validate::<FORFastFieldSerializer, FORFastFieldReader>(data, name)
    }

    #[test]
    fn test_compression() {
        let data = (10..=6_000_u64).collect::<Vec<_>>();
        let (estimate, actual_compression) =
            create_and_validate(&data, "simple monotonically large");
        println!("{}", actual_compression);
        assert!(actual_compression < 0.2);
        assert!(actual_compression > 0.006);
        assert!(estimate < 0.20);
        // assert!(estimate > 0.15);
    }

    #[test]
    fn test_with_codec_data_sets() {
        let data_sets = get_codec_test_data_sets();
        for (mut data, name) in data_sets {
            create_and_validate(&data, name);
            data.reverse();
            create_and_validate(&data, name);
        }
    }
    #[test]
    fn test_simple() {
        let data = (10..=20_u64).collect::<Vec<_>>();
        create_and_validate(&data, "simple monotonically");
    }

    #[test]
    fn border_cases_1() {
        let data = (0..1024).collect::<Vec<_>>();
        create_and_validate(&data, "border case");
    }
    #[test]
    fn border_case_2() {
        let data = (0..1025).collect::<Vec<_>>();
        create_and_validate(&data, "border case");
    }
    #[test]
    fn rand() {
        for _ in 0..10 {
            let mut data = (5_000..20_000)
                .map(|_| rand::random::<u32>() as u64)
                .collect::<Vec<_>>();
            let (estimate, actual_compression) = create_and_validate(&data, "random");
            dbg!(estimate);
            dbg!(actual_compression);

            data.reverse();
            create_and_validate(&data, "random");
        }
    }
}
