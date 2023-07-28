use std::collections::HashMap;
use hnsw_rs_thousand_birds::hnsw::{Hnsw, Neighbour};
use hnsw_rs_thousand_birds::dist::DistDot;

// TODO: manage multiple independent named collections
pub struct InMemoryVectorDb<T> {
    db: HashMap<usize, T>,
    id_counter: usize,
    hnsw: Hnsw::<f32, DistDot>
}

impl<T> InMemoryVectorDb<T> where T: Clone{
    pub fn new() -> Self {
        let mut hnsw = Hnsw::<f32, DistDot>::new(
            // max_nb_connection (in hnsw initialization) The maximum number of links from one
            // point to others. Values ranging from 16 to 64 are standard initialising values,
            // the higher the more time consuming.
            16,

            100_000,

            // max_layer (in hnsw initialization)
            // The maximum number of layers in graph. Must be less or equal than 16.
            16,

            // ef_construction (in hnsw initialization)
            // This parameter controls the width of the search for neighbours during insertion.
            // Values from 200 to 800 are standard initialising values, the higher the more time consuming.
            200,

            // Distance function
            DistDot{}
        );
        hnsw.set_extend_candidates(true);
        Self {
            db: HashMap::new(),
            id_counter: 0,
            hnsw
        }
    }

    pub fn insert(&mut self, data: &Vec<(&Vec<f32>, T)>) {
        // usize is the id
        let mut insert_set = vec![];
        for item in data {
            self.id_counter += 1;
            self.db.insert(self.id_counter, item.1.clone());
            insert_set.push((item.0, self.id_counter));
        }
        self.hnsw.parallel_insert(&insert_set);
    }

    pub fn search(&mut self, data: Vec<f32>, num_neighbors: usize) -> Vec<(Neighbour, T)> {
        self.hnsw.set_searching_mode(true);
        let mut results = vec![];
        // TODO: supports searching multiple keys at once, we should support that
        let neighbors = self.hnsw.parallel_search(&vec![data], num_neighbors, 16);
        for neighbor in neighbors.first().unwrap() {
           results.push((neighbor.clone(), self.db.get(&neighbor.d_id).unwrap().clone()));
        }
        results
    }

}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_db() {
        let mut db = InMemoryVectorDb::new();
        let embedding = vec![0.1, 0.2, 0.3];
        let contents = HashMap::from([("name", "test")]);
        let row = vec![(&embedding, contents)];
        db.insert(&row);
        let search = vec![0.1, 0.1, 0.1];
        let result = db.search(search, 1);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, HashMap::from([("name", "test")]));
    }
}