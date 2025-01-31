// Copyright 2017-2018 Brett Bowman, Jeff Knaggs
// Licensed under the MIT license (http://opensource.org/licenses/MIT)
// This file may not be copied, modified, or distributed
// except according to those terms.

//! Partial-Order Alignment for fast alignment and consensus of multiple homologous sequences.
//!
//! - time complexity: `O(N^2 * L^2)`, where `N` is the number of sequences and `L` is the length of each sequence.
//!
//! For the original concept and theory, see:
//! * Lee, Christopher, Catherine Grasso, and Mark F. Sharlow. "Multiple sequence alignment using
//! partial order graphs." Bioinformatics 18.3 (2002): 452-464.
//! * Lee, Christopher. "Generating consensus sequences from partial order multiple sequence
//! alignment graphs." Bioinformatics 19.8 (2003): 999-1008.
//!
//! For a modern reference implementation, see poapy:
//! https://github.com/ljdursi/poapy
//!
//! # Example
//!
//! ```
//! use bio::alignment::pairwise::Scoring;
//! use bio::alignment::poa::*;
//!
//! let x = b"AAAAAAA";
//! let y = b"AABBBAA";
//! let z = b"AABCBAA";
//!
//! let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
//! let mut aligner = Aligner::new(scoring, x);
//! // z differs from x in 3 locations
//! assert_eq!(aligner.global(z).alignment().score, 1);
//! aligner.global(y).add_to_graph();
//! // z differs from x and y's partial order alignment by 1 base
//! assert_eq!(aligner.global(z).alignment().score, 5);
//! ```

use std::cmp::{max, Ordering};

use crate::utils::TextSlice;

use crate::alignment::pairwise::{MatchFunc, Scoring};

use petgraph::graph::NodeIndex;
use petgraph::visit::Topo;

use petgraph::{Directed, Graph, Incoming};

pub const MIN_SCORE: i32 = -858_993_459; // negative infinity; see alignment/pairwise/mod.rs
pub type POAGraph = Graph<u8, i32, Directed, usize>;

// Unlike with a total order we may have arbitrary successors in the
// traceback matrix. I have not yet figured out what the best level of
// detail to store is, so Match and Del operations remember In and Out
// nodes on the reference graph.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize)]
pub enum AlignmentOperation {
    Match(Option<(usize, usize)>),
    Del(Option<(usize, usize)>),
    Ins(Option<usize>),
}

#[derive(Default, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize)]
pub struct Alignment {
    pub score: i32,
    //    xstart: Edge,
    operations: Vec<AlignmentOperation>,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct TracebackCell {
    score: i32,
    op: AlignmentOperation,
}

impl Ord for TracebackCell {
    fn cmp(&self, other: &TracebackCell) -> Ordering {
        self.score.cmp(&other.score)
    }
}

impl PartialOrd for TracebackCell {
    fn partial_cmp(&self, other: &TracebackCell) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for TracebackCell {
    fn eq(&self, other: &TracebackCell) -> bool {
        self.score == other.score
    }
}

//impl Default for TracebackCell { }

impl Eq for TracebackCell {}

#[derive(Default, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub struct Traceback {
    rows: usize,
    cols: usize,

    // store the last visited node in topological order so that
    // we can index into the end of the alignment when we backtrack
    last: NodeIndex<usize>,
    matrix: Vec<(Vec<TracebackCell>, usize, usize)>,
}

impl Traceback {
    /// Create a Traceback matrix with given maximum sizes
    ///
    /// # Arguments
    ///
    /// * `m` - the number of nodes in the DAG
    /// * `n` - the length of the query sequence
    fn with_capacity(m: usize, n: usize) -> Self {
        // each row of matrix contain start end position and vec of traceback cells
        let matrix: Vec<(Vec<TracebackCell>, usize, usize)> = vec![(vec![], 0, n); m + 1];
        Traceback {
            rows: m,
            cols: n,
            last: NodeIndex::new(0),
            matrix,
        }
    }

    /// Populate the first row of the traceback matrix
    fn initialize_scores(&mut self, gap_open: i32) {
        for j in 0..=self.cols {
            self.matrix[0].0.push(TracebackCell {
                score: (j as i32) * gap_open,
                op: AlignmentOperation::Ins(None),
            });
        }
        self.matrix[0].0[0] = TracebackCell {
            score: 0,
            op: AlignmentOperation::Match(None),
        };
    }

    fn new() -> Self {
        Traceback {
            rows: 0,
            cols: 0,
            last: NodeIndex::new(0),
            matrix: Vec::new(),
        }
    }

    // create a new row according to the parameters
    fn new_row(&mut self, row: usize, size: usize, gap_open: i32, start: usize, end: usize) {
        self.matrix[row].1 = start;
        self.matrix[row].2 = end;
        // when the row starts from the edge
        if start == 0 {
            self.matrix[row].0.push(TracebackCell {
                score: (row as i32) * gap_open,
                op: AlignmentOperation::Del(None),
            });
        } else {
            self.matrix[row].0.push(TracebackCell {
                score: MIN_SCORE,
                op: AlignmentOperation::Match(None),
            });
        }
        for _ in 1..=size {
            self.matrix[row].0.push(TracebackCell {
                score: MIN_SCORE,
                op: AlignmentOperation::Match(None),
            });
        }
    }

    fn set(&mut self, i: usize, j: usize, cell: TracebackCell) {
        // set the matrix cell if in band range
        if !(self.matrix[i].1 > j || self.matrix[i].2 < j) {
            let real_position = j - self.matrix[i].1;
            self.matrix[i].0[real_position] = cell;
        }
    }

    fn get(&self, i: usize, j: usize) -> &TracebackCell {
        // get the matrix cell if in band range else return the appropriate values
        if !(self.matrix[i].1 > j || self.matrix[i].2 < j) {
            let real_position = j - self.matrix[i].1;
            return &self.matrix[i].0[real_position];
        }
        // behind the band, met the edge
        else if j == 0 {
            return &TracebackCell {
                score: MIN_SCORE,
                op: AlignmentOperation::Del(None),
            };
        }
        // infront of the band
        else if j > self.matrix[i].2 {
            return &TracebackCell {
                score: MIN_SCORE,
                op: AlignmentOperation::Ins(None),
            };
        }
        // behind the band
        else {
            return &TracebackCell {
                score: MIN_SCORE,
                op: AlignmentOperation::Match(None),
            };
        }
    }

    pub fn alignment(&self) -> Alignment {
        // optimal AlignmentOperation path
        let mut ops: Vec<AlignmentOperation> = vec![];

        // Now backtrack through the matrix to construct an optimal path
        let mut i = self.last.index() + 1;
        let mut j = self.cols;

        while i > 0 || j > 0 {
            // push operation and edge corresponding to (one of the) optimal
            // routes
            ops.push(self.get(i, j).op.clone());
            match self.get(i, j).op {
                AlignmentOperation::Match(Some((p, _))) => {
                    i = p + 1;
                    j -= 1;
                }
                AlignmentOperation::Del(Some((p, _))) => {
                    i = p + 1;
                }
                AlignmentOperation::Ins(Some(p)) => {
                    i = p + 1;
                    j -= 1;
                }
                AlignmentOperation::Match(None) => {
                    i -= 1;
                    j -= 1;
                }
                AlignmentOperation::Del(None) => {
                    i -= 1;
                }
                AlignmentOperation::Ins(None) => {
                    j -= 1;
                }
            }
        }

        ops.reverse();

        Alignment {
            score: self.get(self.last.index() + 1, self.cols).score,
            operations: ops,
        }
    }
}

/// A partially ordered aligner builder
///
/// Uses consuming builder pattern for constructing partial order alignments with method chaining
#[derive(Default, Clone, Debug)]
pub struct Aligner<F: MatchFunc> {
    traceback: Traceback,
    query: Vec<u8>,
    poa: Poa<F>,
}

impl<F: MatchFunc> Aligner<F> {
    /// Create new instance.
    pub fn new(scoring: Scoring<F>, reference: TextSlice) -> Self {
        Aligner {
            traceback: Traceback::new(),
            query: reference.to_vec(),
            poa: Poa::from_string(scoring, reference),
        }
    }

    /// Add the alignment of the last query to the graph.
    pub fn add_to_graph(&mut self) -> &mut Self {
        let alignment = self.traceback.alignment();
        self.poa.add_alignment(&alignment, &self.query);
        self
    }

    /// Return alignment of last added query against the graph.
    pub fn alignment(&self) -> Alignment {
        self.traceback.alignment()
    }

    /// Globally align a given query against the graph.
    pub fn global(&mut self, query: TextSlice) -> &mut Self {
        self.query = query.to_vec();
        self.traceback = self.poa.global(query);
        self
    }

    /// Globally align a given query against the graph with a band around the previous
    /// optimal score for speed.
    pub fn global_banded(&mut self, query: TextSlice, bandwidth: usize) -> &mut Self {
        self.query = query.to_vec();
        self.traceback = self.poa.global_banded(query, bandwidth);
        self
    }

    /// Return alignment graph.
    pub fn graph(&self) -> &POAGraph {
        &self.poa.graph
    }
    /// Return the consensus sequence generated from the POA graph.
    pub fn consensus(&self) -> Vec<u8> {
        let mut consensus: Vec<u8> = vec![];
        let max_index = self.poa.graph.node_count();
        let mut weight_score_next_vec: Vec<(i32, i32, usize)> = vec![(0, 0, 0); max_index + 1];
        let mut topo = Topo::new(&self.poa.graph);
        // go through the nodes topologically
        while let Some(node) = topo.next(&self.poa.graph) {
            let mut best_weight_score_next: (i32, i32, usize) = (0, 0, usize::MAX);
            let mut neighbour_nodes = self.poa.graph.neighbors_directed(node, Incoming);
            // go through the incoming neighbour nodes
            while let Some(neighbour_node) = neighbour_nodes.next() {
                let mut weight = 0;
                let neighbour_index = neighbour_node.index();
                let neighbour_score = weight_score_next_vec[neighbour_index].1;
                let mut edges = self.poa.graph.edges_connecting(neighbour_node, node);
                while let Some(edge) = edges.next() {
                    weight += edge.weight().clone();
                }
                let current_node_score = weight + neighbour_score;
                // save the neighbour node with the highest weight and score as best
                if (weight, current_node_score, neighbour_index) > best_weight_score_next {
                    best_weight_score_next = (weight, current_node_score, neighbour_index);
                }
            }
            weight_score_next_vec[node.index()] = best_weight_score_next;
        }
        // get the index of the max scored node (end of consensus)
        let mut pos = weight_score_next_vec
            .iter()
            .enumerate()
            .max_by_key(|(_, &value)| value.1)
            .map(|(idx, _)| idx)
            .unwrap();
        // go through weight_score_next_vec appending to the consensus
        while pos != usize::MAX {
            consensus.push(self.poa.graph.raw_nodes()[pos].weight);
            pos = weight_score_next_vec[pos].2;
        }
        consensus.reverse();
        consensus
    }
}

/// A partially ordered alignment graph
///
/// A directed acyclic graph datastructure that represents the topology of a
/// traceback matrix.
#[derive(Default, Clone, Debug)]
pub struct Poa<F: MatchFunc> {
    scoring: Scoring<F>,
    pub graph: POAGraph,
}

impl<F: MatchFunc> Poa<F> {
    /// Create a new aligner instance from the directed acyclic graph of another.
    ///
    /// # Arguments
    ///
    /// * `scoring` - the score struct
    /// * `poa` - the partially ordered reference alignment
    pub fn new(scoring: Scoring<F>, graph: POAGraph) -> Self {
        Poa { scoring, graph }
    }

    /// Create a new POA graph from an initial reference sequence and alignment penalties.
    ///
    /// # Arguments
    ///
    /// * `scoring` - the score struct
    /// * `reference` - a reference TextSlice to populate the initial reference graph
    pub fn from_string(scoring: Scoring<F>, seq: TextSlice) -> Self {
        let mut graph: Graph<u8, i32, Directed, usize> =
            Graph::with_capacity(seq.len(), seq.len() - 1);
        let mut prev: NodeIndex<usize> = graph.add_node(seq[0]);
        let mut node: NodeIndex<usize>;
        for base in seq.iter().skip(1) {
            node = graph.add_node(*base);
            graph.add_edge(prev, node, 1);
            prev = node;
        }

        Poa { scoring, graph }
    }

    /// A global Needleman-Wunsch aligner on partially ordered graphs.
    ///
    /// # Arguments
    /// * `query` - the query TextSlice to align against the internal graph member
    pub fn global(&self, query: TextSlice) -> Traceback {
        assert!(self.graph.node_count() != 0);

        // dimensions of the traceback matrix
        let (m, n) = (self.graph.node_count(), query.len());
        let mut traceback = Traceback::with_capacity(m, n);
        traceback.initialize_scores(self.scoring.gap_open);

        // construct the score matrix (O(n^2) space)
        let mut topo = Topo::new(&self.graph);
        while let Some(node) = topo.next(&self.graph) {
            // reference base and index
            let r = self.graph.raw_nodes()[node.index()].weight; // reference base at previous index
            let i = node.index() + 1; // 0 index is for initialization so we start at 1
            traceback.last = node;
            // iterate over the predecessors of this node
            let prevs: Vec<NodeIndex<usize>> =
                self.graph.neighbors_directed(node, Incoming).collect();
            traceback.new_row(i, n + 1, self.scoring.gap_open, 0, n);
            // query base and its index in the DAG (traceback matrix rows)
            for (query_index, query_base) in query.iter().enumerate() {
                let j = query_index + 1; // 0 index is initialized so we start at 1
                                         // match and deletion scores for the first reference base
                let max_cell = if prevs.is_empty() {
                    TracebackCell {
                        score: traceback.get(0, j - 1).score
                            + self.scoring.match_fn.score(r, *query_base),
                        op: AlignmentOperation::Match(None),
                    }
                } else {
                    let mut max_cell = TracebackCell {
                        score: MIN_SCORE,
                        op: AlignmentOperation::Match(None),
                    };
                    for prev_node in &prevs {
                        let i_p: usize = prev_node.index() + 1; // index of previous node
                        max_cell = max(
                            max_cell,
                            max(
                                TracebackCell {
                                    score: traceback.get(i_p, j - 1).score
                                        + self.scoring.match_fn.score(r, *query_base),
                                    op: AlignmentOperation::Match(Some((i_p - 1, i - 1))),
                                },
                                TracebackCell {
                                    score: traceback.get(i_p, j).score + self.scoring.gap_open,
                                    op: AlignmentOperation::Del(Some((i_p - 1, i))),
                                },
                            ),
                        );
                    }
                    max_cell
                };

                let score = max(
                    max_cell,
                    TracebackCell {
                        score: traceback.get(i, j - 1).score + self.scoring.gap_open,
                        op: AlignmentOperation::Ins(Some(i - 1)),
                    },
                );
                traceback.set(i, j, score);
            }
        }

        traceback
    }

    /// A global Needleman-Wunsch aligner on partially ordered graphs with banding.
    ///
    /// # Arguments
    /// * `query` - the query TextSlice to align against the internal graph member
    /// * `bandwidth` - width of band, if too small, alignment may be suboptimal
    pub fn global_banded(&self, query: TextSlice, bandwidth: usize) -> Traceback {
        assert!(self.graph.node_count() != 0);

        // dimensions of the traceback matrix
        let (m, n) = (self.graph.node_count(), query.len());
        let mut traceback = Traceback::with_capacity(m, n);
        traceback.initialize_scores(self.scoring.gap_open);

        traceback.set(
            0,
            0,
            TracebackCell {
                score: 0,
                op: AlignmentOperation::Match(None),
            },
        );

        // construct the score matrix (O(n^2) space)
        // but this sucks, we want linear time!!!
        // at each row i we want to find the max scoring j
        // and band
        let mut max_scoring_j = 0;
        let mut max_score_for_row = MIN_SCORE;
        let mut topo = Topo::new(&self.graph);
        while let Some(node) = topo.next(&self.graph) {
            // reference base and index
            let r = self.graph.raw_nodes()[node.index()].weight; // reference base at previous index
            let i = node.index() + 1; // 0 index is for initialization so we start at 1
            traceback.last = node;
            // iterate over the predecessors of this node
            let prevs: Vec<NodeIndex<usize>> =
                self.graph.neighbors_directed(node, Incoming).collect();
            let start = if bandwidth > max_scoring_j {
                0
            } else {
                max_scoring_j - bandwidth
            };
            let end = max_scoring_j + bandwidth;
            traceback.new_row(i, (end - start) + 1, self.scoring.gap_open, start, end);
            for (query_index, query_base) in query.iter().enumerate().skip(start) {
                let j = query_index + 1; // 0 index is initialized so we start at 1
                if j > end {
                    break;
                }
                let max_cell = if prevs.is_empty() {
                    TracebackCell {
                        score: traceback.get(0, j - 1).score
                            + self.scoring.match_fn.score(r, *query_base),
                        op: AlignmentOperation::Match(None),
                    }
                } else {
                    let mut max_cell = TracebackCell {
                        score: MIN_SCORE,
                        op: AlignmentOperation::Match(None),
                    };
                    for prev_node in &prevs {
                        let i_p: usize = prev_node.index() + 1; // index of previous node
                        max_cell = max(
                            max_cell,
                            max(
                                TracebackCell {
                                    score: traceback.get(i_p, j - 1).score
                                        + self.scoring.match_fn.score(r, *query_base),
                                    op: AlignmentOperation::Match(Some((i_p - 1, i - 1))),
                                },
                                TracebackCell {
                                    score: traceback.get(i_p, j).score + self.scoring.gap_open,
                                    op: AlignmentOperation::Del(Some((i_p - 1, i))),
                                },
                            ),
                        );
                    }
                    max_cell
                };

                let score = max(
                    max_cell,
                    TracebackCell {
                        score: traceback.get(i, j - 1).score + self.scoring.gap_open,
                        op: AlignmentOperation::Ins(Some(i - 1)),
                    },
                );
                if score.score > max_score_for_row {
                    max_scoring_j = j;
                    max_score_for_row = score.score;
                }
                traceback.set(i, j, score);
            }
        }

        traceback
    }

    /// Experimental: return sequence of traversed edges
    ///
    /// Only supports alignments for sequences that have already been added,
    /// so all operations must be Match.
    pub fn edges(&self, aln: Alignment) -> Vec<usize> {
        let mut path: Vec<usize> = vec![];
        let mut prev: NodeIndex<usize> = NodeIndex::new(0);
        let mut _i: usize = 0;
        for op in aln.operations {
            match op {
                AlignmentOperation::Match(None) => {
                    _i += 1;
                }
                AlignmentOperation::Match(Some((_, p))) => {
                    let node = NodeIndex::new(p);
                    let edge = self.graph.find_edge(prev, node).unwrap();
                    path.push(edge.index());
                    prev = NodeIndex::new(p);
                    _i += 1;
                }
                AlignmentOperation::Ins(None) => {}
                AlignmentOperation::Ins(Some(_)) => {}
                AlignmentOperation::Del(_) => {}
            }
        }
        path
    }

    /// Incorporate a new sequence into a graph from an alignment
    ///
    /// # Arguments
    ///
    /// * `aln` - The alignment of the new sequence to the graph
    /// * `seq` - The sequence being incorporated
    pub fn add_alignment(&mut self, aln: &Alignment, seq: TextSlice) {
        let head = Topo::new(&self.graph).next(&self.graph).unwrap();
        let mut prev: NodeIndex<usize> = NodeIndex::new(head.index());
        let mut i: usize = 0;
        let mut edge_not_connected: bool = false;
        for op in aln.operations.iter() {
            match op {
                AlignmentOperation::Match(None) => {
                    let node: NodeIndex<usize> = NodeIndex::new(0);
                    if (seq[i] != self.graph.raw_nodes()[head.index()].weight) && (seq[i] != b'X') {
                        let node = self.graph.add_node(seq[i]);
                        prev = node;
                    }
                    if edge_not_connected {
                        self.graph.add_edge(prev, node, 1);
                        prev = node;
                        edge_not_connected = false;
                    }
                    i += 1;
                }
                AlignmentOperation::Match(Some((_, p))) => {
                    let node = NodeIndex::new(*p);
                    if (seq[i] != self.graph.raw_nodes()[*p].weight) && (seq[i] != b'X') {
                        let node = self.graph.add_node(seq[i]);
                        self.graph.add_edge(prev, node, 1);
                        prev = node;
                    } else {
                        // increment node weight
                        match self.graph.find_edge(prev, node) {
                            Some(edge) => {
                                *self.graph.edge_weight_mut(edge).unwrap() += 1;
                            }
                            None => {
                                if prev.index() != head.index() {
                                    self.graph.add_edge(prev, node, 1);
                                }
                            }
                        }
                        prev = NodeIndex::new(*p);
                    }
                    i += 1;
                }
                AlignmentOperation::Ins(None) => {
                    let node = self.graph.add_node(seq[i]);
                    if edge_not_connected {
                        self.graph.add_edge(prev, node, 1);
                    }
                    prev = node;
                    edge_not_connected = true;
                    i += 1;
                }
                AlignmentOperation::Ins(Some(_)) => {
                    let node = self.graph.add_node(seq[i]);
                    self.graph.add_edge(prev, node, 1);
                    prev = node;
                    i += 1;
                }
                AlignmentOperation::Del(_) => {} // we should only have to skip over deleted nodes
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alignment::pairwise::Scoring;
    use petgraph::dot::Dot;
    use petgraph::graph::NodeIndex;

    #[test]
    fn test_init_graph() {
        // sanity check for String -> Graph

        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
        let poa = Poa::from_string(scoring, b"123456789");
        assert!(poa.graph.is_directed());
        assert_eq!(poa.graph.node_count(), 9);
        assert_eq!(poa.graph.edge_count(), 8);
    }

    #[test]
    fn test_alignment() {
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
        // examples from the POA paper
        //let _seq1 = b"PKMIVRPQKNETV";
        //let _seq2 = b"THKMLVRNETIM";
        let poa = Poa::from_string(scoring, b"GATTACA");
        let alignment = poa.global(b"GCATGCU").alignment();
        assert_eq!(alignment.score, 0);

        let alignment = poa.global(b"GCATGCUx").alignment();
        assert_eq!(alignment.score, -1);

        let alignment = poa.global(b"xCATGCU").alignment();
        assert_eq!(alignment.score, -2);
    }

    #[test]
    fn test_branched_alignment() {
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
        let seq1 = b"TTTTT";
        let seq2 = b"TTATT";
        let mut poa = Poa::from_string(scoring, seq1);
        let head: NodeIndex<usize> = NodeIndex::new(1);
        let tail: NodeIndex<usize> = NodeIndex::new(2);
        let node1 = poa.graph.add_node(b'A');
        let node2 = poa.graph.add_node(b'A');
        poa.graph.add_edge(head, node1, 1);
        poa.graph.add_edge(node1, node2, 1);
        poa.graph.add_edge(node2, tail, 1);
        let alignment = poa.global(seq2).alignment();
        assert_eq!(alignment.score, 3);
    }

    #[test]
    fn test_alt_branched_alignment() {
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });

        let seq1 = b"TTCCTTAA";
        let seq2 = b"TTTTGGAA";
        let mut poa = Poa::from_string(scoring, seq1);
        let head: NodeIndex<usize> = NodeIndex::new(1);
        let tail: NodeIndex<usize> = NodeIndex::new(2);
        let node1 = poa.graph.add_node(b'A');
        let node2 = poa.graph.add_node(b'A');
        poa.graph.add_edge(head, node1, 1);
        poa.graph.add_edge(node1, node2, 1);
        poa.graph.add_edge(node2, tail, 1);
        let alignment = poa.global(seq2).alignment();
        poa.add_alignment(&alignment, seq2);
        assert_eq!(poa.graph.edge_count(), 14);
        assert!(poa
            .graph
            .contains_edge(NodeIndex::new(5), NodeIndex::new(10)));
        assert!(poa
            .graph
            .contains_edge(NodeIndex::new(11), NodeIndex::new(6)));
    }

    #[test]
    fn test_insertion_on_branch() {
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });

        let seq1 = b"TTCCGGTTTAA";
        let seq2 = b"TTGGTATGGGAA";
        let seq3 = b"TTGGTTTGCGAA";
        let mut poa = Poa::from_string(scoring, seq1);
        let head: NodeIndex<usize> = NodeIndex::new(1);
        let tail: NodeIndex<usize> = NodeIndex::new(2);
        let node1 = poa.graph.add_node(b'C');
        let node2 = poa.graph.add_node(b'C');
        let node3 = poa.graph.add_node(b'C');
        poa.graph.add_edge(head, node1, 1);
        poa.graph.add_edge(node1, node2, 1);
        poa.graph.add_edge(node2, node3, 1);
        poa.graph.add_edge(node3, tail, 1);
        let alignment = poa.global(seq2).alignment();
        assert_eq!(alignment.score, 2);
        poa.add_alignment(&alignment, seq2);
        let alignment2 = poa.global(seq3).alignment();

        assert_eq!(alignment2.score, 10);
    }

    #[test]
    fn test_poa_method_chaining() {
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
        let mut aligner = Aligner::new(scoring, b"TTCCGGTTTAA");
        aligner
            .global(b"TTGGTATGGGAA")
            .add_to_graph()
            .global(b"TTGGTTTGCGAA")
            .add_to_graph();
        assert_eq!(aligner.alignment().score, 10);
    }

    #[test]
    fn test_global_banded() {
        // need strings long enough for the band to matter
        // create MSA with and without band and make sure they are the same
        let s1 = b"TGGCATGCTCAAGGACCGTTGAATACTATCTTAATGGACCGCAAGCTCCCTGAAGGTGGGCCACATTCGAGGGCC\
        CGGCCTCCACCTATTCCCAACGAAACTAGCATTAACATGGACAGGGGCGCATAAAACAGAGTTTCTCCTAATCCCCTTTCCCCTG\
        GAGTGCTAGTCAGAACCGCACATGTTGACGCTTTGGTCAGGTGTAGCCGATTCACTACCCGGGGTAGTACGAGTGGTAGCACCAT\
        GGTTAGCTTCTCCGGGATGTTCCGCGAAGAGAGCGGAGCGGGCGTGCACAAGCTCGGACAACCCTAGTGTGCATCAAATGCCATA\
        TGTTCTGCTTTGTCTGTGACTCACGCCCACGTTTGACATCACTCTTACTATCCAACGGGCCAAGCTTAGGAGGGGCGGACCTATT\
        GAACCATTAGAGGGGATCCTTCTGAAGTTAAGGCACAGCGTTGAGGGGCTATAGTCGATCCTCTTAGTAAATATAATGGACAGGT\
        CTTTACGACACAGTATGAATTAGTCCAATGGAGCCATTGTAATCGATGAAACTGTTATATCTGTTGGCCTAGTCGCAACGGTCTA\
        CATCGCTAGCGTAACGGTTAAGACCTCTTCCACGAGTGGGACACTCATAAAGCTCGCGGCCCTTACGATCTAGGGGAGCGCACTC\
        CGTAGTCAATCACGGCCAGCCGGTGTGCGCTAAGTTACGAAACAGTCACGAGCGATGAACCGTATGAAGAATGGACCCTTCTAAG\
        ATGTGAACACCTAGATGAGCAGCAAGACAATTGCTCTCGCCGACTCGTTCGAAAGTGTACCTCGAGAGCACAACACGCATTACCC\
        AGGTGACCGTGTATTGACTGCCCGTTACTCAGAAACCTTACAGTATTAATCGCCTAGTCTGTATAGTATTCATTCTGCCCGTGAC
        ATGCGGGAAGCCTGCTGAGATTGGCAGCGTCTTTGGAGGGTTACCAAGCGAGGACACGGGCAAATTGAGGTGT";
        let s2 = b"TGGCTACATGCTCAAGCATCGTTGAAGCTCATCTTAATGGACCGCAACGGCCGCCTGAAGGTGGGACACGTGACG\
        GGCGGGGGCCCGGCCTTAACCCATTCTCAAGCAACTAGCATACTGGACAGCGGCGCATATACAGAGAATCGCCTAAACCCACTTT\
        TGCCCTGAGTGCTAGTCAGTCCCCCACATCTGACACTTCGGTGGCGCACGTTTAGCAGCTTACACTACCCGGGGCAGTACGAGTG\
        CTAGCACGGTAGCCTCCGGAGGGCTGCGAGGAATAGAACGGAGAGGGCGTCCTCAAGCCGGACAACCCTAGTGTGCATCAAATGA\
        TGCCTGCTGATTTTCTGTGCATTTCACGCCCAATTCACAATCACTCCTACTATCCAACGGGCAAGCATAAGGGAGGGGGGGAGTA\
        CGTCTATTGCACCATTAGAGGGGTACTTCGAATTCGTTGAACTGAGATAGAGTCGATCCTCTTTGTATATAAACGCAGGTACTTT\
        GCTATAAGGTGAATTATTCAAATGGAGCCATTGTAATCGATGACAATGTTATACCTTTAGGCCTAGATCAACGGTCTCCATCGCA\
        AGCGTAACGATTATGACCCAACGAGTGGACACTCATAGAGCGGCCCTTACGAGCTAGGCGAGCGCAATCCGTGTGAATCACAGCC\
        AGACGGGATGTTGCGTTAAGCTACGAAACATCACGCGGTGAGCGTATGAATAATGGACCCGTCAAAATGTGGGCAGCGAGCAGCA\
        GGACAATTGCTCGGGTCCGGTAGCGACTCGTTCGAAACTGTAAACGTCGAGGCACAACACGCATTAGCCAGGTGAACATGTATTG\
        ACCGCCCCGTAGATACCTTACAGTATTAATCGCCTAGTCTGTATGATCTTCGTTCTGCCTGTGAACATGAGGGAAGCCTGCTTGA\
        GTTTGGCAGCGTCTTTGGGGTTTCCAAGCGAGCGACACGGGCAAATTGAGGTGT";
        let s3 = b"TGGCATGCTCAAGGAGTGCTGAAGCTCATTTTAATGGACCGCAACGGCCGCCTGAAGGTGGGGCACGTGACGGGC\
        GAGGGCCCGGCCTTAACCCATTCTCAAGAAACTCGTATACTGGACAGCGGCGCATATAGAGAGATTCTCCTAAACCCTCTTTTGC\
        CCTGACATGTGCTAGTCAGTCGCCCACATCTGAACACTTCGGCAGCGCACGTCTAGCAGCTTACACTACCGGGCGGGGCAGGTAC\
        GAGTGCTAGCACGGTAGCCTCTCCCGGAGTGCTGGGAATAGAAGGGAGAGGGCGTCCTCATGCCGGCGACCCTAGTGTGCATCAA\
        ATGAGATGCCTGCTGTGATTTTCACATTCACAATCACTCTTACCATCCCAACGGGACAAGCATAAGGGAGGGGGGGAGCTATTGA\
        ACCAAGAGGGGTCCTCCGGAATTCGTTGAGCTGCGATAGAGTCGATCCTCTTTGTATATAAACGCAGGTACTTTGCGATTAGGTG\
        AAGTATTCAAATGGAGCCATTGTAATCGATGACAATGTGATGCCTTTAGGCCTAGATCACGGTCTACATCGCGTAAGCGTAACGA\
        TTATGACCCAACGAGAGGCACACTCATAAAGCGCGGCCCTTACTAGCTAGGCGAGCTTAGCAATCCGTGCAATCACACCCAGACG\
        GGTTGAGCTAAGCTACGGAACACCACGCGATGAGCCGTATGAAGAATGGACCCGTCGAAAATGTGGACAGCGAGCATCAGGACAA\
        TTGCTCGGGTCCGCGACTCGTGCGGAACTGTAAACGTCGAGGCACAACACGATTAGCCAGGTGAACATGTAGACCGCCCCGTAGA\
        TATTTTACAGTATTAATCGCCTAGTCTGTATAGGATCTTCGTTCTGCCTGTGAACATGCGGGAAGCCTGCTTGAGATTGGCAGCG\
        TCTTTGGGCAAGCGAGGACACGGGCAAATCGAGGTGG";
        let scoring = Scoring::from_scores(-2, -2, 2, -4);
        let mut aligner_banded = Aligner::new(scoring, s1);
        aligner_banded.global_banded(s2, 25).add_to_graph();
        aligner_banded.global_banded(s3, 25).add_to_graph();
        let scoring = Scoring::from_scores(-2, -2, 2, -4);
        let mut aligner_unbanded = Aligner::new(scoring, s1);
        aligner_unbanded.global(s2).add_to_graph();
        aligner_unbanded.global(s3).add_to_graph();
        let alignment_banded = aligner_banded.alignment();
        let alignment_unbanded = aligner_unbanded.alignment();
        for (i, operation) in alignment_banded.operations.iter().enumerate() {
            assert_eq!(*operation, alignment_unbanded.operations[i]);
        }
    }

    #[test]
    fn test_edge_cases() {
        // case 1
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
        let mut aligner = Aligner::new(scoring, b"BBA");
        aligner.global(b"AAA").add_to_graph();
        let g = aligner.graph().map(|_, n| (*n) as char, |_, e| *e);
        let dot = format!("{:?}", Dot::new(&g));
        let output = "digraph {\n    0 [ label = \"'B'\" ]\n    1 [ label = \"'B'\" ]\n    2 [ label = \"'A'\" ]\
        \n    3 [ label = \"'A'\" ]\n    4 [ label = \"'A'\" ]\n    0 -> 1 [ label = \"1\" ]\n    1 -> 2 [ label = \"1\" ]\
        \n    3 -> 4 [ label = \"1\" ]\n    4 -> 2 [ label = \"1\" ]\n}\n".to_string();
        assert_eq!(dot, output);
        // case 2
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
        let mut aligner = Aligner::new(scoring, b"AAA");
        aligner.global(b"ABA").add_to_graph();
        let g = aligner.graph().map(|_, n| (*n) as char, |_, e| *e);
        let dot = format!("{:?}", Dot::new(&g));
        let output = "digraph {\n    0 [ label = \"'A'\" ]\n    1 [ label = \"'A'\" ]\n    2 [ label = \"'A'\" ]\
        \n    3 [ label = \"'B'\" ]\n    0 -> 1 [ label = \"1\" ]\n    1 -> 2 [ label = \"1\" ]\n    0 -> 3 [ label = \"1\" ]\
        \n    3 -> 2 [ label = \"1\" ]\n}\n".to_string();
        assert_eq!(dot, output);
        // case 3
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
        let mut aligner = Aligner::new(scoring, b"BBBBBAAA");
        aligner.global(b"AAA").add_to_graph();
        let g = aligner.graph().map(|_, n| (*n) as char, |_, e| *e);
        let dot = format!("{:?}", Dot::new(&g));
        let output = "digraph {\n    0 [ label = \"'B'\" ]\n    1 [ label = \"'B'\" ]\n    2 [ label = \"'B'\" ]\
        \n    3 [ label = \"'B'\" ]\n    4 [ label = \"'B'\" ]\n    5 [ label = \"'A'\" ]\n    6 [ label = \"'A'\" ]\
        \n    7 [ label = \"'A'\" ]\n    0 -> 1 [ label = \"1\" ]\n    1 -> 2 [ label = \"1\" ]\n    2 -> 3 [ label = \"1\" ]\
        \n    3 -> 4 [ label = \"1\" ]\n    4 -> 5 [ label = \"1\" ]\n    5 -> 6 [ label = \"2\" ]\n    6 -> 7 [ label = \"2\" ]\n}\n".to_string();
        assert_eq!(dot, output);
        // case 4
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
        let mut aligner = Aligner::new(scoring, b"AAA");
        aligner.global(b"BBBBBAAA").add_to_graph();
        let g = aligner.graph().map(|_, n| (*n) as char, |_, e| *e);
        let dot = format!("{:?}", Dot::new(&g));
        let output = "digraph {\n    0 [ label = \"'A'\" ]\n    1 [ label = \"'A'\" ]\n    2 [ label = \"'A'\" ]\
        \n    3 [ label = \"'B'\" ]\n    4 [ label = \"'B'\" ]\n    5 [ label = \"'B'\" ]\n    6 [ label = \"'B'\" ]\
        \n    7 [ label = \"'B'\" ]\n    0 -> 1 [ label = \"2\" ]\n    1 -> 2 [ label = \"2\" ]\n    3 -> 4 [ label = \"1\" ]\
        \n    4 -> 5 [ label = \"1\" ]\n    5 -> 6 [ label = \"1\" ]\n    6 -> 7 [ label = \"1\" ]\n    7 -> 0 [ label = \"1\" ]\n}\n".to_string();
        assert_eq!(dot, output);
    }
    #[test]
    fn test_consensus() {
        let scoring = Scoring::new(-1, 0, |a: u8, b: u8| if a == b { 1i32 } else { -1i32 });
        let mut aligner = Aligner::new(scoring, b"GCATGCUx");
        aligner.global(b"GCATGCU").add_to_graph();
        aligner.global(b"xCATGCU").add_to_graph();
        assert_eq!(aligner.consensus(), b"GCATGCUx");
    }
}
