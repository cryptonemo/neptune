use crate::batch_hasher::{Batcher, BatcherType};
use crate::error::Error;
use crate::poseidon::{Poseidon, PoseidonConstants};
use crate::BatchHasher;
use bellperson::bls::{Bls12, Fr};
use ff::Field;
use generic_array::{typenum, GenericArray};
#[cfg(all(feature = "gpu", not(target_os = "macos")))]
use rust_gpu_tools::opencl::GPUSelector;
use smallvec::SmallVec;

pub trait TreeBuilderTrait<TreeArity>
where
    TreeArity: typenum::Unsigned,
{
    fn add_leaves(&mut self, leaves: &[Fr]) -> Result<(), Error>;
    fn add_final_leaves(&mut self, leaves: &[Fr]) -> Result<(Vec<Fr>, Vec<Fr>), Error>;

    fn reset(&mut self);
}

pub struct TreeBuilder<TreeArity>
where
    TreeArity: typenum::Unsigned,
{
    pub leaf_count: usize,
    data: Vec<Fr>,
    /// Index of the first unfilled datum.
    fill_index: usize,
    tree_constants: PoseidonConstants<Bls12, TreeArity>,
    tree_batcher: Option<Batcher<TreeArity>>,
    rows_to_discard: usize,
}

impl<TreeArity> TreeBuilderTrait<TreeArity> for TreeBuilder<TreeArity>
where
    TreeArity: typenum::Unsigned,
{
    fn add_leaves(&mut self, leaves: &[Fr]) -> Result<(), Error> {
        let start = self.fill_index;
        let batch_leaf_count = leaves.len();
        let end = start + batch_leaf_count;

        if end > self.leaf_count {
            return Err(Error::Other("too many leaves".to_string()));
        }

        self.data[start..end].copy_from_slice(leaves);
        self.fill_index += batch_leaf_count;

        Ok(())
    }

    fn add_final_leaves(&mut self, leaves: &[Fr]) -> Result<(Vec<Fr>, Vec<Fr>), Error> {
        self.add_leaves(leaves)?;

        let res = self.build_tree(self.rows_to_discard);
        self.reset();

        res
    }

    fn reset(&mut self) {
        self.fill_index = 0;
        self.data.iter_mut().for_each(|place| *place = Fr::zero());
    }
}

impl<TreeArity> TreeBuilder<TreeArity>
where
    TreeArity: typenum::Unsigned,
{
    pub fn new(
        t: Option<BatcherType>,
        leaf_count: usize,
        max_tree_batch_size: usize,
        rows_to_discard: usize,
    ) -> Result<Self, Error> {
        let builder = Self {
            leaf_count,
            data: vec![Fr::zero(); leaf_count],
            fill_index: 0,
            tree_constants: PoseidonConstants::<Bls12, TreeArity>::new(),
            tree_batcher: if let Some(t) = &t {
                Some(Batcher::<TreeArity>::new(t, max_tree_batch_size)?)
            } else {
                None
            },
            rows_to_discard,
        };

        // Cannot discard the base row or the root.
        assert!(rows_to_discard < builder.tree_height());

        // This will panic if leaf_count is not compatible with tree arity.
        // That is the desired behavior so such a programmer error is caught at development time.
        let _ = builder.tree_size(rows_to_discard);

        Ok(builder)
    }

    pub fn build_tree(&mut self, rows_to_discard: usize) -> Result<(Vec<Fr>, Vec<Fr>), Error> {
        let final_tree_size = self.tree_size(rows_to_discard);
        let intermediate_tree_size = self.tree_size(0) + self.leaf_count;
        let arity = TreeArity::to_usize();

        let mut tree_data = vec![Fr::zero(); intermediate_tree_size];

        tree_data[0..self.leaf_count].copy_from_slice(&self.data);

        let (mut start, mut end) = (0, arity);

        match &mut self.tree_batcher {
            Some(batcher) => {
                let max_batch_size = batcher.max_batch_size();

                let (mut row_start, mut row_end) = (0, self.leaf_count);
                while row_end < intermediate_tree_size {
                    let row_size = row_end - row_start;
                    assert_eq!(0, row_size % arity);
                    let new_row_size = row_size / arity;
                    let (new_row_start, new_row_end) = (row_end, row_end + new_row_size);

                    let mut total_hashed = 0;
                    let mut batch_start = row_start;

                    while total_hashed < new_row_size {
                        let batch_end = usize::min(batch_start + (max_batch_size * arity), row_end);
                        let batch_size = (batch_end - batch_start) / arity;
                        let (current, new) = tree_data.split_at_mut(new_row_start + total_hashed);

                        let hashed = batcher
                            .hash(current[batch_start..batch_end].chunks(TreeArity::to_usize()))?;
                        new[..hashed.len()].copy_from_slice(&hashed);
                        total_hashed += batch_size;
                        batch_start = batch_end;
                        drop(current);
                        drop(new);
                    }

                    row_start = new_row_start;
                    row_end = new_row_end;
                }
            }
            None => {
                for i in self.leaf_count..intermediate_tree_size {
                    tree_data[i] =
                        Poseidon::new_with_preimage(&tree_data[start..end], &self.tree_constants)
                            .hash();
                    start += arity;
                    end += arity;
                }
            }
        }

        let base_row = tree_data[..self.leaf_count].to_vec();
        let tree_to_keep = tree_data[tree_data.len() - final_tree_size..].to_vec();
        Ok((base_row, tree_to_keep))
    }

    /// `tree_size` returns the number of nodes in the tree to cache.
    /// This excludes the base row and the following `rows_to_discard` rows.
    pub fn tree_size(&self, rows_to_discard: usize) -> usize {
        let arity = TreeArity::to_usize();

        let mut tree_size = 0;
        let mut current_row_size = self.leaf_count;

        // Exclude the base row, along with the rows to be discarded.
        let mut remaining_rows_to_exclude = rows_to_discard + 1;

        while current_row_size >= 1 {
            if remaining_rows_to_exclude > 0 {
                remaining_rows_to_exclude -= 1;
            } else {
                tree_size += current_row_size;
            }
            if current_row_size != 1 {
                assert_eq!(
                    0,
                    current_row_size % arity,
                    "Tree leaf count {} is not a power of arity {}.",
                    self.leaf_count,
                    arity
                )
            }
            current_row_size /= arity;
        }

        tree_size
    }

    pub fn tree_height(&self) -> usize {
        let arity = TreeArity::to_usize();

        let mut tree_height = 0;
        let mut current_row_size = self.leaf_count;

        // Could also just calculate log base arity directly.
        while current_row_size >= 1 {
            if current_row_size != 1 {
                tree_height += 1;
                assert_eq!(
                    0,
                    current_row_size % arity,
                    "Tree leaf count {} is not a power of arity {}.",
                    self.leaf_count,
                    arity
                );
            }
            current_row_size /= arity;
        }
        tree_height
    }

    // Compute root of tree composed of all identical columns. For use in checking correctness of GPU tree-building
    // without the cost of generating a full tree.
    pub fn compute_uniform_tree_root(&mut self, leaf: Fr) -> Result<Fr, Error> {
        let arity = TreeArity::to_usize();
        let mut element = leaf;
        for _ in 0..self.tree_height() {
            let preimage = vec![element; arity];
            // Each row is the hash of the identical elements in the previous row.
            element = Poseidon::new_with_preimage(&preimage, &self.tree_constants).hash();
        }

        // The last element computed is the root.
        Ok(element)
    }
}

#[cfg(all(feature = "gpu", not(target_os = "macos")))]
#[cfg(test)]
mod tests {
    use super::*;
    use bellperson::bls::Fr;
    use ff::Field;
    use generic_array::typenum::U8;

    #[test]
    fn test_tree_builder() {
        // 16KiB tree has 512 leaves.
        test_tree_builder_aux(None, 512, 32, 512, 512);
        test_tree_builder_aux(Some(BatcherType::CPU), 512, 32, 512, 512);

        #[cfg(all(feature = "gpu", not(target_os = "macos")))]
        test_tree_builder_aux(Some(BatcherType::GPU), 512, 32, 512, 512);
    }

    fn test_tree_builder_aux(
        batcher_type: Option<BatcherType>,
        leaves: usize,
        num_batches: usize,
        max_leaf_batch_size: usize,
        max_tree_batch_size: usize,
    ) {
        let batch_size = leaves / num_batches;

        for rows_to_discard in 0..3 {
            let mut builder = TreeBuilder::<U8>::new(
                batcher_type.clone(),
                leaves,
                max_tree_batch_size,
                rows_to_discard,
            )
            .unwrap();

            // Simplify computing the expected root.
            let constant_element = Fr::zero();

            let effective_batch_size = usize::min(batch_size, max_leaf_batch_size);

            let mut total_leaves = 0;
            while total_leaves + effective_batch_size < leaves {
                let leaves: Vec<Fr> = (0..effective_batch_size)
                    .map(|_| constant_element)
                    .collect();

                let _ = builder.add_leaves(leaves.as_slice()).unwrap();
                total_leaves += leaves.len();
            }

            let final_leaves: Vec<_> = (0..leaves - total_leaves)
                .map(|_| constant_element)
                .collect();

            let (base, res) = builder.add_final_leaves(final_leaves.as_slice()).unwrap();

            let computed_root = res[res.len() - 1];

            let expected_root = builder.compute_uniform_tree_root(final_leaves[0]).unwrap();
            let expected_size = builder.tree_size(rows_to_discard);

            assert_eq!(leaves, base.len());
            assert!(base.iter().all(|x| *x == constant_element));

            assert_eq!(expected_size, res.len());
            assert_eq!(expected_root, computed_root);
        }
    }
}
