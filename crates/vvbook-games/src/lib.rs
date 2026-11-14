#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod chess;
pub mod pocket_pip;

pub use chess::{CHESS_PUZZLES, ChessApp, ChessEngine, ChessMode, ChessMove, Color, GameStatus, Piece, Position};
pub use pocket_pip::{Dir, GameAction, MEADOW_LEVELS, MeadowState, PipProgress, PipTab, PocketPipApp, solve_meadow_bfs};
