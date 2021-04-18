use crate::vector_storage::vector_storage::{VectorStorage, ScoredPointOffset, RawScorer};
use crate::entry::entry_point::OperationResult;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::fs::{File, OpenOptions, create_dir_all};
use memmap::{MmapOptions, Mmap, MmapMut};
use std::mem::{size_of, transmute};
use crate::types::{VectorElementType, PointOffsetType, Distance};
use std::io::Write;
use crate::spaces::tools::{mertic_object, peek_top_scores};
use crate::common::error_logging::LogError;
use crate::spaces::metric::Metric;
use crate::vector_storage::mmap_vectors::MmapVectors;


fn vf_to_u8<T>(v: &Vec<T>) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * size_of::<T>()) }
}

pub struct MemmapRawScorer<'a> {
    query: Vec<VectorElementType>,
    metric: Box<dyn Metric>,
    mmap_store: &'a MmapVectors,
}

impl RawScorer for MemmapRawScorer<'_> {
    fn score_points<'a>(&'a self, points: &'a mut dyn Iterator<Item=PointOffsetType>) -> Box<dyn Iterator<Item=ScoredPointOffset> + 'a> {
        let res_iter = points
            .filter(move |point| !self.mmap_store.deleted(*point).unwrap_or(true))
            .map(move |point| {
                let other_vector = self.mmap_store.raw_vector(point).unwrap();
                ScoredPointOffset {
                    idx: point,
                    score: self.metric.similarity(&self.query, other_vector),
                }
            });
        Box::new(res_iter)
    }
}


pub struct MemmapVectorStorage {
    vectors_path: PathBuf,
    deleted_path: PathBuf,
    mmap_store: Option<MmapVectors>,
}

impl MemmapVectorStorage {
    pub fn open(path: &Path, dim: usize) -> OperationResult<Self> {
        create_dir_all(path)?;

        let vectors_path = path.join("matrix.dat");
        let deleted_path = path.join("deleted.dat");

        let mmap_store = MmapVectors::open(
            vectors_path.as_path(),
            deleted_path.as_path(),
            dim,
        )?;

        Ok(MemmapVectorStorage {
            vectors_path,
            deleted_path,
            mmap_store: Some(mmap_store),
        })
    }
}


impl VectorStorage for MemmapVectorStorage {
    fn vector_dim(&self) -> usize {
        self.mmap_store.as_ref().unwrap().dim
    }

    fn vector_count(&self) -> usize {
        self.mmap_store.as_ref().map(|store| store.num_vectors - store.deleted_count).unwrap()
    }

    fn deleted_count(&self) -> usize {
        self.mmap_store.as_ref().unwrap().deleted_count
    }

    fn get_vector(&self, key: PointOffsetType) -> Option<Vec<VectorElementType>> {
        self.mmap_store.as_ref().and_then(|x| x.get_vector(key))
    }

    fn put_vector(&mut self, _vector: &Vec<VectorElementType>) -> OperationResult<PointOffsetType> {
        panic!("Can't put vector in mmap storage")
    }

    fn update_vector(&mut self, _key: usize, _vector: &Vec<VectorElementType>) -> OperationResult<usize> {
        panic!("Can't directly update vector in mmap storage")
    }

    fn update_from(&mut self, other: &dyn VectorStorage) -> OperationResult<Range<PointOffsetType>> {
        let dim = self.vector_dim();

        let start_index = self.mmap_store.as_ref().unwrap().num_vectors;
        let mut end_index = start_index;

        self.mmap_store = None;

        {
            let mut file = OpenOptions::new()
                .read(false)
                .write(false)
                .append(true)
                .create(false)
                .open(self.vectors_path.as_path())?;

            for id in other.iter_ids() {
                let vector = &other.get_vector(id).unwrap();
                let raw_bites = vf_to_u8(vector);
                file.write(raw_bites)?;
                end_index += 1;
            }

            file.flush()?;
        }
        {
            let mut file = OpenOptions::new()
                .read(false)
                .write(false)
                .append(true)
                .create(false)
                .open(self.deleted_path.as_path())?;

            let flags: Vec<u8> = vec![0; end_index - start_index];
            let flag_bytes = vf_to_u8(&flags);
            file.write(flag_bytes)?;
            file.flush()?;
        }

        self.mmap_store = Some(MmapVectors::open(
            self.vectors_path.as_path(),
            self.deleted_path.as_path(),
            dim,
        )?);

        return Ok(start_index..end_index);
    }

    fn delete(&mut self, key: PointOffsetType) -> OperationResult<()> {
        self.mmap_store.as_mut().unwrap().delete(key)
    }

    fn iter_ids(&self) -> Box<dyn Iterator<Item=PointOffsetType> + '_> {
        let num_vectors = self.mmap_store.as_ref().unwrap().num_vectors;
        let iter = (0..num_vectors)
            .filter(move |id| !self.mmap_store.as_ref().unwrap().deleted(*id).unwrap());
        return Box::new(iter);
    }

    fn flush(&self) -> OperationResult<()> {
        match self.mmap_store.as_ref() {
            None => Ok(()),
            Some(x) => x.flush()
        }
    }

    fn raw_scorer(&self, vector: &Vec<VectorElementType>, distance: &Distance) -> Box<dyn RawScorer + '_> {
        let metric = mertic_object(distance);
        let query = metric.preprocess(vector.clone());
        Box::new(
            MemmapRawScorer {
                query,
                metric,
                mmap_store: &self.mmap_store.as_ref().unwrap()
            }
        )
    }

    fn score_points(
        &self, vector: &Vec<VectorElementType>,
        points: &[PointOffsetType],
        top: usize,
        distance: &Distance,
    ) -> Vec<ScoredPointOffset> {
        let metric = mertic_object(distance);
        let preprocessed_vector = metric.preprocess(vector.clone());
        let scores: Vec<ScoredPointOffset> = points.iter()
            .cloned()
            .filter(|point| !self.mmap_store.as_ref().unwrap().deleted(*point).unwrap_or(true))
            .map(|point| {
                let other_vector = self.mmap_store.as_ref().unwrap().raw_vector(point).unwrap();
                ScoredPointOffset {
                    idx: point,
                    score: metric.similarity(&preprocessed_vector, &other_vector),
                }
            }).collect();
        return peek_top_scores(&scores, top, distance);
    }

    fn score_all(&self, vector: &Vec<VectorElementType>, top: usize, distance: &Distance) -> Vec<ScoredPointOffset> {
        let metric = mertic_object(distance);
        let preprocessed_vector = metric.preprocess(vector.clone());
        let scores: Vec<ScoredPointOffset> = self.iter_ids()
            .map(|point| {
                let other_vector = self.mmap_store.as_ref().unwrap().raw_vector(point).unwrap();
                ScoredPointOffset {
                    idx: point,
                    score: metric.similarity(&preprocessed_vector, other_vector),
                }
            }).collect();

        return peek_top_scores(&scores, top, distance);
    }

    fn score_internal(
        &self,
        point: PointOffsetType,
        points: &[PointOffsetType],
        top: usize,
        distance: &Distance,
    ) -> Vec<ScoredPointOffset> {
        let vector = self.get_vector(point).unwrap();
        return self.score_points(&vector, points, top, distance);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;
    use std::mem::transmute;
    use crate::vector_storage::simple_vector_storage::SimpleVectorStorage;
    use itertools::Itertools;

    #[test]
    fn test_basic_persistence() {
        let dir = TempDir::new("storage_dir").unwrap();
        let mut storage = MemmapVectorStorage::open(dir.path(), 4).unwrap();

        let vec1 = vec![1.0, 0.0, 1.0, 1.0];
        let vec2 = vec![1.0, 0.0, 1.0, 0.0];
        let vec3 = vec![1.0, 1.0, 1.0, 1.0];
        let vec4 = vec![1.0, 1.0, 0.0, 1.0];
        let vec5 = vec![1.0, 0.0, 0.0, 0.0];

        {
            let dir2 = TempDir::new("storage_dir2").unwrap();
            let mut storage2 = SimpleVectorStorage::open(dir2.path(), 4).unwrap();

            storage2.put_vector(&vec1).unwrap();
            storage2.put_vector(&vec2).unwrap();
            storage2.put_vector(&vec3).unwrap();
            storage.update_from(&storage2).unwrap();
        }

        assert_eq!(storage.vector_count(), 3);

        let vector = storage.get_vector(1).unwrap();

        assert_eq!(vec2, vector);

        assert_eq!(storage.deleted_count(), 0);

        storage.delete(2).unwrap();

        {
            let dir2 = TempDir::new("storage_dir2").unwrap();
            let mut storage2 = SimpleVectorStorage::open(dir2.path(), 4).unwrap();
            storage2.put_vector(&vec4).unwrap();
            storage2.put_vector(&vec5).unwrap();
            storage.update_from(&storage2).unwrap();
        }

        assert_eq!(storage.vector_count(), 4);


        let stored_ids: Vec<PointOffsetType> = storage.iter_ids().collect();

        assert_eq!(stored_ids, vec![0, 1, 3, 4]);


        let res = storage.score_all(&vec3, 2, &Distance::Dot);

        assert_eq!(res.len(), 2);

        assert_ne!(res[0].idx, 2);

        let res = storage.score_points(
            &vec3, &vec![0, 1, 2, 3, 4], 2, &Distance::Dot);

        assert_eq!(res.len(), 2);
        assert_ne!(res[0].idx, 2);
    }

    #[test]
    fn test_mmap_raw_scorer() {
        let dir = TempDir::new("storage_dir").unwrap();
        let mut storage = MemmapVectorStorage::open(dir.path(), 4).unwrap();

        let vec1 = vec![1.0, 0.0, 1.0, 1.0];
        let vec2 = vec![1.0, 0.0, 1.0, 0.0];
        let vec3 = vec![1.0, 1.0, 1.0, 1.0];
        let vec4 = vec![1.0, 1.0, 0.0, 1.0];
        let vec5 = vec![1.0, 0.0, 0.0, 0.0];

        {
            let dir2 = TempDir::new("storage_dir2").unwrap();
            let mut storage2 = SimpleVectorStorage::open(dir2.path(), 4).unwrap();

            storage2.put_vector(&vec1).unwrap();
            storage2.put_vector(&vec2).unwrap();
            storage2.put_vector(&vec3).unwrap();
            storage2.put_vector(&vec4).unwrap();
            storage2.put_vector(&vec5).unwrap();
            storage.update_from(&storage2).unwrap();
        }

        let query = vec![-1.0, -1.0, -1.0, -1.0];
        let query_points: Vec<PointOffsetType> = vec![0, 2, 4];

        let scorer = storage.raw_scorer(&query, &Distance::Dot);

        let res = scorer.score_points(&mut query_points.iter().cloned()).collect_vec();

        assert_eq!(res.len(), 3);
        assert_eq!(res[0].idx, 0);
        assert_eq!(res[1].idx, 2);
        assert_eq!(res[2].idx, 4);

        assert_eq!(res[2].score, -1.0);
    }


    #[test]
    fn test_casts() {
        let data: Vec<VectorElementType> = vec![0.42, 0.069, 333.1, 100500.];

        let raw_data = vf_to_u8(&data);

        eprintln!("raw_data.len() = {:#?}", raw_data.len());


        let arr: &[VectorElementType] = unsafe { transmute(raw_data) };

        let slice = &arr[0..data.len()];

        eprintln!("slice.len() = {:#?}", slice.len());

        for (idx, element) in slice.iter().enumerate() {
            println!("slice[{}]  = {:?}", idx, element);
        }
    }
}