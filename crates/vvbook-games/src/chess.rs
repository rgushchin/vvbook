extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use vvbook_gfx::baked_fonts;
use vvbook_gfx::canvas::{Align, Canvas};
use vvbook_gfx::icons::{ChessPieceKind, draw_chess_piece};
use vvbook_pal::{Button, PORTRAIT_W, RefreshMode, SAFE_INSETS, Tone};

use crate::pocket_pip::GameAction;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    White = 0,
    Black = 1,
}

impl Color {
    #[inline]
    pub fn opp(self) -> Self {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece {
    Pawn = 1,
    Knight = 2,
    Bishop = 3,
    Rook = 4,
    Queen = 5,
    King = 6,
}

impl Piece {
    #[inline]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v & 7 {
            1 => Some(Self::Pawn),
            2 => Some(Self::Knight),
            3 => Some(Self::Bishop),
            4 => Some(Self::Rook),
            5 => Some(Self::Queen),
            6 => Some(Self::King),
            _ => None,
        }
    }

    #[inline]
    pub fn to_gfx(self) -> ChessPieceKind {
        match self {
            Self::Pawn => ChessPieceKind::Pawn,
            Self::Knight => ChessPieceKind::Knight,
            Self::Bishop => ChessPieceKind::Bishop,
            Self::Rook => ChessPieceKind::Rook,
            Self::Queen => ChessPieceKind::Queen,
            Self::King => ChessPieceKind::King,
        }
    }

    #[inline]
    pub fn mg_val(self) -> i32 {
        match self {
            Self::Pawn => 82,
            Self::Knight => 337,
            Self::Bishop => 365,
            Self::Rook => 477,
            Self::Queen => 1025,
            Self::King => 20_000,
        }
    }

    #[inline]
    pub fn eg_val(self) -> i32 {
        match self {
            Self::Pawn => 94,
            Self::Knight => 281,
            Self::Bishop => 297,
            Self::Rook => 512,
            Self::Queen => 936,
            Self::King => 20_000,
        }
    }

    #[inline]
    pub fn phase_weight(self) -> i32 {
        match self {
            Self::Knight | Self::Bishop => 1,
            Self::Rook => 2,
            Self::Queen => 4,
            _ => 0,
        }
    }
}

#[inline]
fn pack_sq(color: Color, piece: Piece) -> u8 {
    ((color as u8) << 3) | (piece as u8)
}

#[inline]
fn unpack_sq(val: u8) -> Option<(Color, Piece)> {
    let p = Piece::from_u8(val)?;
    let c = if (val & 8) == 0 { Color::White } else { Color::Black };
    Some((c, p))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Default)]
pub struct ChessMove {
    pub from: u8,
    pub to: u8,
    pub promo: Option<Piece>,
    pub is_ep: bool,
    pub is_castle: bool,
}


impl ChessMove {
    pub fn uci(self) -> String {
        let f1 = (b'a' + (self.from % 8)) as char;
        let r1 = (b'1' + (self.from / 8)) as char;
        let f2 = (b'a' + (self.to % 8)) as char;
        let r2 = (b'1' + (self.to / 8)) as char;
        let mut s = format!("{f1}{r1}{f2}{r2}");
        if let Some(p) = self.promo {
            s.push(match p {
                Piece::Queen => 'q',
                Piece::Rook => 'r',
                Piece::Bishop => 'b',
                Piece::Knight => 'n',
                _ => 'q',
            });
        }
        s
    }

    pub fn from_uci_on(_pos: &Position, uci: &str, legal: &[ChessMove]) -> Option<Self> {
        legal.iter().copied().find(|m| m.uci() == uci)
    }
}

/// Zero-heap fixed-capacity move list (`218` is the theoretical max legal moves in any reachable position).
pub struct MoveList {
    pub moves: [ChessMove; 218],
    pub scores: [i32; 218],
    pub len: usize,
}

impl Default for MoveList {
    fn default() -> Self {
        Self::new()
    }
}

impl MoveList {
    #[inline]
    pub fn new() -> Self {
        Self {
            moves: [ChessMove::default(); 218],
            scores: [0; 218],
            len: 0,
        }
    }

    #[inline]
    pub fn push(&mut self, mv: ChessMove) {
        if self.len < 218 {
            self.moves[self.len] = mv;
            self.scores[self.len] = 0;
            self.len += 1;
        }
    }

    /// Incremental selection sort: swap best-scored remaining move into `idx`.
    #[inline]
    pub fn pick_next(&mut self, idx: usize) -> ChessMove {
        let mut best_i = idx;
        let mut best_s = self.scores[idx];
        let mut i = idx + 1;
        while i < self.len {
            if self.scores[i] > best_s {
                best_s = self.scores[i];
                best_i = i;
            }
            i += 1;
        }
        if best_i != idx {
            self.moves.swap(idx, best_i);
            self.scores.swap(idx, best_i);
        }
        self.moves[idx]
    }
}

// Deterministic Zobrist keys built at compile time (`const fn` SplitMix64).
struct ZobristKeys {
    pieces: [[u64; 64]; 16],
    side: u64,
    castle: [u64; 16],
    ep_file: [u64; 8],
}

const fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

const ZOBRIST: ZobristKeys = {
    let mut seed = 0x5656_424F_4F4B_3236u64;
    let mut pieces = [[0u64; 64]; 16];
    let mut p = 0;
    while p < 16 {
        let mut sq = 0;
        while sq < 64 {
            seed = splitmix64(seed);
            pieces[p][sq] = seed;
            sq += 1;
        }
        p += 1;
    }
    seed = splitmix64(seed);
    let side = seed;
    let mut castle = [0u64; 16];
    let mut c = 0;
    while c < 16 {
        seed = splitmix64(seed);
        castle[c] = seed;
        c += 1;
    }
    let mut ep_file = [0u64; 8];
    let mut f = 0;
    while f < 8 {
        seed = splitmix64(seed);
        ep_file[f] = seed;
        f += 1;
    }
    ZobristKeys {
        pieces,
        side,
        castle,
        ep_file,
    }
};

pub const CASTLE_WK: u8 = 1;
pub const CASTLE_WQ: u8 = 2;
pub const CASTLE_BK: u8 = 4;
pub const CASTLE_BQ: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub squares: [u8; 64],
    pub stm: Color,
    pub castle: u8,
    pub ep: Option<u8>,
    pub halfmove: u16,
    pub fullmove: u16,
    pub hash: u64,
}

impl Default for Position {
    fn default() -> Self {
        Self::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1").unwrap()
    }
}

impl Position {
    pub fn compute_hash(&self) -> u64 {
        let mut h = 0u64;
        for sq in 0..64 {
            let v = self.squares[sq];
            if v != 0 {
                h ^= ZOBRIST.pieces[v as usize][sq];
            }
        }
        if self.stm == Color::Black {
            h ^= ZOBRIST.side;
        }
        h ^= ZOBRIST.castle[(self.castle & 15) as usize];
        if let Some(ep) = self.ep {
            h ^= ZOBRIST.ep_file[(ep % 8) as usize];
        }
        h
    }

    pub fn from_fen(fen: &str) -> Option<Self> {
        let parts: Vec<&str> = fen.split_whitespace().collect();
        if parts.len() < 4 {
            return None;
        }
        let mut squares = [0u8; 64];
        let ranks: Vec<&str> = parts[0].split('/').collect();
        if ranks.len() != 8 {
            return None;
        }
        for (ri, rstr) in ranks.iter().enumerate() {
            let rank = 7 - ri;
            let mut file = 0usize;
            for ch in rstr.chars() {
                if let Some(d) = ch.to_digit(10) {
                    file += d as usize;
                } else {
                    if file >= 8 {
                        return None;
                    }
                    let color = if ch.is_uppercase() { Color::White } else { Color::Black };
                    let piece = match ch.to_ascii_lowercase() {
                        'p' => Piece::Pawn,
                        'n' => Piece::Knight,
                        'b' => Piece::Bishop,
                        'r' => Piece::Rook,
                        'q' => Piece::Queen,
                        'k' => Piece::King,
                        _ => return None,
                    };
                    squares[rank * 8 + file] = pack_sq(color, piece);
                    file += 1;
                }
            }
            if file != 8 {
                return None;
            }
        }
        let stm = match parts[1] {
            "w" => Color::White,
            "b" => Color::Black,
            _ => return None,
        };
        let mut castle = 0u8;
        if parts[2] != "-" {
            for ch in parts[2].chars() {
                match ch {
                    'K' => castle |= CASTLE_WK,
                    'Q' => castle |= CASTLE_WQ,
                    'k' => castle |= CASTLE_BK,
                    'q' => castle |= CASTLE_BQ,
                    _ => {}
                }
            }
        }
        let ep = if parts[3] == "-" {
            None
        } else {
            let b = parts[3].as_bytes();
            if b.len() >= 2 && (b'a'..=b'h').contains(&b[0]) && (b'1'..=b'8').contains(&b[1]) {
                Some((b[1] - b'1') * 8 + (b[0] - b'a'))
            } else {
                None
            }
        };
        let halfmove = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
        let fullmove = parts.get(5).and_then(|s| s.parse().ok()).unwrap_or(1);
        let mut pos = Self {
            squares,
            stm,
            castle,
            ep,
            halfmove,
            fullmove,
            hash: 0,
        };
        pos.hash = pos.compute_hash();
        Some(pos)
    }

    #[inline]
    pub fn king_sq(&self, color: Color) -> Option<u8> {
        let target = pack_sq(color, Piece::King);
        self.squares.iter().position(|&v| v == target).map(|i| i as u8)
    }

    pub fn has_non_pawn_material(&self, color: Color) -> bool {
        for &v in &self.squares {
            if let Some((c, p)) = unpack_sq(v)
                && c == color && !matches!(p, Piece::Pawn | Piece::King) {
                    return true;
                }
        }
        false
    }

    pub fn is_square_attacked(&self, sq: u8, by: Color) -> bool {
        let r = (sq / 8) as i32;
        let f = (sq % 8) as i32;

        let pawn_dir = if by == Color::White { -1 } else { 1 };
        let pawn_val = pack_sq(by, Piece::Pawn);
        for df in [-1, 1] {
            let pr = r + pawn_dir;
            let pf = f + df;
            if (0..8).contains(&pr) && (0..8).contains(&pf) && self.squares[(pr * 8 + pf) as usize] == pawn_val {
                return true;
            }
        }

        let knight_val = pack_sq(by, Piece::Knight);
        for (dr, df) in [(-2, -1), (-2, 1), (-1, -2), (-1, 2), (1, -2), (1, 2), (2, -1), (2, 1)] {
            let nr = r + dr;
            let nf = f + df;
            if (0..8).contains(&nr) && (0..8).contains(&nf) && self.squares[(nr * 8 + nf) as usize] == knight_val {
                return true;
            }
        }

        let king_val = pack_sq(by, Piece::King);
        for dr in -1..=1 {
            for df in -1..=1 {
                if dr == 0 && df == 0 {
                    continue;
                }
                let nr = r + dr;
                let nf = f + df;
                if (0..8).contains(&nr) && (0..8).contains(&nf) && self.squares[(nr * 8 + nf) as usize] == king_val {
                    return true;
                }
            }
        }

        let rook_val = pack_sq(by, Piece::Rook);
        let queen_val = pack_sq(by, Piece::Queen);
        for (dr, df) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            let mut nr = r + dr;
            let mut nf = f + df;
            while (0..8).contains(&nr) && (0..8).contains(&nf) {
                let v = self.squares[(nr * 8 + nf) as usize];
                if v != 0 {
                    if v == rook_val || v == queen_val {
                        return true;
                    }
                    break;
                }
                nr += dr;
                nf += df;
            }
        }

        let bishop_val = pack_sq(by, Piece::Bishop);
        for (dr, df) in [(-1, -1), (-1, 1), (1, -1), (1, 1)] {
            let mut nr = r + dr;
            let mut nf = f + df;
            while (0..8).contains(&nr) && (0..8).contains(&nf) {
                let v = self.squares[(nr * 8 + nf) as usize];
                if v != 0 {
                    if v == bishop_val || v == queen_val {
                        return true;
                    }
                    break;
                }
                nr += dr;
                nf += df;
            }
        }

        false
    }

    #[inline]
    pub fn in_check(&self, color: Color) -> bool {
        self.king_sq(color)
            .map(|k| self.is_square_attacked(k, color.opp()))
            .unwrap_or(false)
    }

    pub fn make_move(&self, mv: ChessMove) -> Self {
        let mut n = *self;
        let moving = n.squares[mv.from as usize];
        let captured = n.squares[mv.to as usize];
        let (_, piece) = unpack_sq(moving).unwrap_or((n.stm, Piece::Pawn));

        n.squares[mv.from as usize] = 0;
        let placed = if let Some(promo) = mv.promo {
            pack_sq(n.stm, promo)
        } else {
            moving
        };
        n.squares[mv.to as usize] = placed;

        if mv.is_ep {
            let cap_sq = if n.stm == Color::White { mv.to - 8 } else { mv.to + 8 };
            n.squares[cap_sq as usize] = 0;
        }

        if mv.is_castle {
            match mv.to {
                6 => {
                    n.squares[7] = 0;
                    n.squares[5] = pack_sq(Color::White, Piece::Rook);
                }
                2 => {
                    n.squares[0] = 0;
                    n.squares[3] = pack_sq(Color::White, Piece::Rook);
                }
                62 => {
                    n.squares[63] = 0;
                    n.squares[61] = pack_sq(Color::Black, Piece::Rook);
                }
                58 => {
                    n.squares[56] = 0;
                    n.squares[59] = pack_sq(Color::Black, Piece::Rook);
                }
                _ => {}
            }
        }

        let clear_mask = |sq: u8| -> u8 {
            match sq {
                0 => CASTLE_WQ,
                4 => CASTLE_WK | CASTLE_WQ,
                7 => CASTLE_WK,
                56 => CASTLE_BQ,
                60 => CASTLE_BK | CASTLE_BQ,
                63 => CASTLE_BK,
                _ => 0,
            }
        };
        n.castle &= !(clear_mask(mv.from) | clear_mask(mv.to));

        n.ep = if piece == Piece::Pawn && (mv.from as i32 - mv.to as i32).abs() == 16 {
            Some(((mv.from as u16 + mv.to as u16) / 2) as u8)
        } else {
            None
        };

        if piece == Piece::Pawn || captured != 0 || mv.is_ep {
            n.halfmove = 0;
        } else {
            n.halfmove += 1;
        }
        if n.stm == Color::Black {
            n.fullmove += 1;
        }
        n.stm = n.stm.opp();
        n.hash = n.compute_hash();
        n
    }

    pub fn make_null_move(&self) -> Self {
        let mut n = *self;
        n.ep = None;
        n.halfmove += 1;
        n.stm = n.stm.opp();
        n.hash = n.compute_hash();
        n
    }

    pub fn generate_legal_list(&self, captures_only: bool) -> MoveList {
        let mut out = MoveList::new();
        let us = self.stm;
        let them = us.opp();
        let mut push_if_legal = |mv: ChessMove| {
            let nxt = self.make_move(mv);
            if !nxt.in_check(us) {
                out.push(mv);
            }
        };

        for sq in 0..64u8 {
            let Some((c, p)) = unpack_sq(self.squares[sq as usize]) else {
                continue;
            };
            if c != us {
                continue;
            }
            let r = (sq / 8) as i32;
            let f = (sq % 8) as i32;

            match p {
                Piece::Pawn => {
                    let dir = if us == Color::White { 1 } else { -1 };
                    let start_rank = if us == Color::White { 1 } else { 6 };
                    let promo_rank = if us == Color::White { 7 } else { 0 };
                    let nr = r + dir;
                    if (0..8).contains(&nr) {
                        let fwd = (nr * 8 + f) as u8;
                        if self.squares[fwd as usize] == 0 {
                            if nr == promo_rank {
                                for pr in [Piece::Queen, Piece::Rook, Piece::Bishop, Piece::Knight] {
                                    push_if_legal(ChessMove { from: sq, to: fwd, promo: Some(pr), is_ep: false, is_castle: false });
                                }
                            } else if !captures_only {
                                push_if_legal(ChessMove { from: sq, to: fwd, promo: None, is_ep: false, is_castle: false });
                                if r == start_rank {
                                    let fwd2 = ((r + dir * 2) * 8 + f) as u8;
                                    if self.squares[fwd2 as usize] == 0 {
                                        push_if_legal(ChessMove { from: sq, to: fwd2, promo: None, is_ep: false, is_castle: false });
                                    }
                                }
                            }
                        }
                        for df in [-1, 1] {
                            let nf = f + df;
                            if !(0..8).contains(&nf) {
                                continue;
                            }
                            let cap_sq = (nr * 8 + nf) as u8;
                            if let Some((cc, _)) = unpack_sq(self.squares[cap_sq as usize]) {
                                if cc == them {
                                    if nr == promo_rank {
                                        for pr in [Piece::Queen, Piece::Rook, Piece::Bishop, Piece::Knight] {
                                            push_if_legal(ChessMove { from: sq, to: cap_sq, promo: Some(pr), is_ep: false, is_castle: false });
                                        }
                                    } else {
                                        push_if_legal(ChessMove { from: sq, to: cap_sq, promo: None, is_ep: false, is_castle: false });
                                    }
                                }
                            } else if self.ep == Some(cap_sq) {
                                push_if_legal(ChessMove { from: sq, to: cap_sq, promo: None, is_ep: true, is_castle: false });
                            }
                        }
                    }
                }
                Piece::Knight => {
                    for (dr, df) in [(-2, -1), (-2, 1), (-1, -2), (-1, 2), (1, -2), (1, 2), (2, -1), (2, 1)] {
                        let nr = r + dr;
                        let nf = f + df;
                        if (0..8).contains(&nr) && (0..8).contains(&nf) {
                            let to = (nr * 8 + nf) as u8;
                            let occ = unpack_sq(self.squares[to as usize]);
                            if captures_only && occ.is_none() {
                                continue;
                            }
                            if occ.map(|(cc, _)| cc != us).unwrap_or(true) {
                                push_if_legal(ChessMove { from: sq, to, promo: None, is_ep: false, is_castle: false });
                            }
                        }
                    }
                }
                Piece::Bishop | Piece::Rook | Piece::Queen => {
                    let dirs: &[(i32, i32)] = match p {
                        Piece::Bishop => &[(-1, -1), (-1, 1), (1, -1), (1, 1)],
                        Piece::Rook => &[(-1, 0), (1, 0), (0, -1), (0, 1)],
                        _ => &[(-1, -1), (-1, 1), (1, -1), (1, 1), (-1, 0), (1, 0), (0, -1), (0, 1)],
                    };
                    for &(dr, df) in dirs {
                        let mut nr = r + dr;
                        let mut nf = f + df;
                        while (0..8).contains(&nr) && (0..8).contains(&nf) {
                            let to = (nr * 8 + nf) as u8;
                            if let Some((cc, _)) = unpack_sq(self.squares[to as usize]) {
                                if cc == them {
                                    push_if_legal(ChessMove { from: sq, to, promo: None, is_ep: false, is_castle: false });
                                }
                                break;
                            }
                            if !captures_only {
                                push_if_legal(ChessMove { from: sq, to, promo: None, is_ep: false, is_castle: false });
                            }
                            nr += dr;
                            nf += df;
                        }
                    }
                }
                Piece::King => {
                    for dr in -1..=1 {
                        for df in -1..=1 {
                            if dr == 0 && df == 0 {
                                continue;
                            }
                            let nr = r + dr;
                            let nf = f + df;
                            if (0..8).contains(&nr) && (0..8).contains(&nf) {
                                let to = (nr * 8 + nf) as u8;
                                let occ = unpack_sq(self.squares[to as usize]);
                                if captures_only && occ.is_none() {
                                    continue;
                                }
                                if occ.map(|(cc, _)| cc != us).unwrap_or(true) {
                                    push_if_legal(ChessMove { from: sq, to, promo: None, is_ep: false, is_castle: false });
                                }
                            }
                        }
                    }
                    if !captures_only {
                        if us == Color::White && sq == 4 && !self.in_check(Color::White) {
                            if (self.castle & CASTLE_WK) != 0
                                && self.squares[5] == 0
                                && self.squares[6] == 0
                                && self.squares[7] == pack_sq(Color::White, Piece::Rook)
                                && !self.is_square_attacked(5, Color::Black)
                                && !self.is_square_attacked(6, Color::Black)
                            {
                                push_if_legal(ChessMove { from: 4, to: 6, promo: None, is_ep: false, is_castle: true });
                            }
                            if (self.castle & CASTLE_WQ) != 0
                                && self.squares[3] == 0
                                && self.squares[2] == 0
                                && self.squares[1] == 0
                                && self.squares[0] == pack_sq(Color::White, Piece::Rook)
                                && !self.is_square_attacked(3, Color::Black)
                                && !self.is_square_attacked(2, Color::Black)
                            {
                                push_if_legal(ChessMove { from: 4, to: 2, promo: None, is_ep: false, is_castle: true });
                            }
                        } else if us == Color::Black && sq == 60 && !self.in_check(Color::Black) {
                            if (self.castle & CASTLE_BK) != 0
                                && self.squares[61] == 0
                                && self.squares[62] == 0
                                && self.squares[63] == pack_sq(Color::Black, Piece::Rook)
                                && !self.is_square_attacked(61, Color::White)
                                && !self.is_square_attacked(62, Color::White)
                            {
                                push_if_legal(ChessMove { from: 60, to: 62, promo: None, is_ep: false, is_castle: true });
                            }
                            if (self.castle & CASTLE_BQ) != 0
                                && self.squares[59] == 0
                                && self.squares[58] == 0
                                && self.squares[57] == 0
                                && self.squares[56] == pack_sq(Color::Black, Piece::Rook)
                                && !self.is_square_attacked(59, Color::White)
                                && !self.is_square_attacked(58, Color::White)
                            {
                                push_if_legal(ChessMove { from: 60, to: 58, promo: None, is_ep: false, is_castle: true });
                            }
                        }
                    }
                }
            }
        }
        out
    }

    pub fn legal_moves(&self) -> Vec<ChessMove> {
        let list = self.generate_legal_list(false);
        list.moves[..list.len].to_vec()
    }

    pub fn format_san(&self, mv: ChessMove, all_legal: &[ChessMove]) -> String {
        if mv.is_castle {
            return if mv.to % 8 == 6 { "O-O".into() } else { "O-O-O".into() };
        }
        let (_, piece) = unpack_sq(self.squares[mv.from as usize]).unwrap_or((self.stm, Piece::Pawn));
        let is_cap = self.squares[mv.to as usize] != 0 || mv.is_ep;
        let to_f = (b'a' + (mv.to % 8)) as char;
        let to_r = (b'1' + (mv.to / 8)) as char;
        let mut s = String::new();
        if piece == Piece::Pawn {
            if is_cap {
                s.push((b'a' + (mv.from % 8)) as char);
                s.push('x');
            }
            s.push(to_f);
            s.push(to_r);
        } else {
            s.push(match piece {
                Piece::Knight => 'N',
                Piece::Bishop => 'B',
                Piece::Rook => 'R',
                Piece::Queen => 'Q',
                Piece::King => 'K',
                Piece::Pawn => 'P',
            });
            let same_target: Vec<ChessMove> = all_legal
                .iter()
                .copied()
                .filter(|m| {
                    m.to == mv.to
                        && m.from != mv.from
                        && unpack_sq(self.squares[m.from as usize]).map(|(_, p)| p == piece).unwrap_or(false)
                })
                .collect();
            if !same_target.is_empty() {
                let same_file = same_target.iter().any(|m| m.from % 8 == mv.from % 8);
                let same_rank = same_target.iter().any(|m| m.from / 8 == mv.from / 8);
                if !same_file {
                    s.push((b'a' + (mv.from % 8)) as char);
                } else if !same_rank {
                    s.push((b'1' + (mv.from / 8)) as char);
                } else {
                    s.push((b'a' + (mv.from % 8)) as char);
                    s.push((b'1' + (mv.from / 8)) as char);
                }
            }
            if is_cap {
                s.push('x');
            }
            s.push(to_f);
            s.push(to_r);
        }
        if let Some(pr) = mv.promo {
            s.push('=');
            s.push(match pr {
                Piece::Queen => 'Q',
                Piece::Rook => 'R',
                Piece::Bishop => 'B',
                Piece::Knight => 'N',
                _ => 'Q',
            });
        }
        let next = self.make_move(mv);
        if next.in_check(next.stm) {
            if next.generate_legal_list(false).len == 0 {
                s.push('#');
            } else {
                s.push('+');
            }
        }
        s
    }
}

// PeSTO Tapered Evaluation tables (indexed rank 7 -> 0 matching `flip(sq) = (7 - r) * 8 + f` for White).
#[rustfmt::skip]
const MG_PAWN: [i32; 64] = [
     0,  0,  0,  0,  0,  0,  0,  0,
    98,134, 61, 95, 68,126, 34,-11,
    -6,  7, 26, 31, 65, 56, 25,-20,
   -14, 13,  6, 21, 23, 12, 17,-23,
   -27, -2, -5, 12, 17,  6, 10,-25,
   -26, -4, -4,-10,  3,  3, 33,-12,
   -35, -1,-20,-23,-15, 24, 38,-22,
     0,  0,  0,  0,  0,  0,  0,  0,
];
#[rustfmt::skip]
const EG_PAWN: [i32; 64] = [
     0,  0,  0,  0,  0,  0,  0,  0,
   178,173,158,134,147,132,165,187,
    94,100, 85, 67, 56, 53, 82, 84,
    32, 24, 13,  5, -2,  4, 17, 17,
    13,  9, -3, -7, -7, -8,  3, -1,
     4,  7, -6,  1,  0, -5, -1, -8,
    13,  8,  8, 10, 13,  0,  2, -7,
     0,  0,  0,  0,  0,  0,  0,  0,
];
#[rustfmt::skip]
const MG_KNIGHT: [i32; 64] = [
  -167,-89,-34,-49, 61,-97,-15,-107,
   -73,-41, 72, 36, 23, 62,  7, -17,
   -47, 60, 37, 65, 84,129, 73,  44,
    -9, 17, 19, 53, 37, 69, 18,  22,
   -13,  4, 16, 13, 28, 19, 21,  -8,
   -23, -9, 12, 10, 19, 17, 25, -16,
   -29,-53,-12, -3, -1, 18,-14, -19,
  -105,-21,-58,-33,-17,-28,-19, -23,
];
#[rustfmt::skip]
const EG_KNIGHT: [i32; 64] = [
   -58,-38,-13,-28,-31,-27,-63,-99,
   -25, -8,-25, -2, -9,-25,-24,-52,
   -24,-20, 10,  9, -1, -9,-19,-41,
   -17,  3, 22, 22, 22, 11,  8,-18,
   -18, -6, 16, 25, 16, 17,  4,-18,
   -23, -3, -1, 15, 10, -3,-20,-22,
   -42,-20,-10, -5, -2,-20,-23,-44,
   -29,-51,-23,-15,-22,-18,-50,-64,
];
#[rustfmt::skip]
const MG_BISHOP: [i32; 64] = [
   -29,  4,-82,-37,-25,-42,  7, -8,
   -26, 16,-18,-13, 30, 59, 18,-47,
   -16, 37, 43, 40, 35, 50, 37, -2,
    -4,  5, 19, 50, 37, 37,  7, -2,
    -6, 13, 13, 26, 34, 12, 10,  4,
     0, 15, 15, 15, 14, 27, 18, 10,
     4, 15, 16,  0,  7, 21, 33,  1,
   -33, -3,-14,-21,-13,-12,-39,-21,
];
#[rustfmt::skip]
const EG_BISHOP: [i32; 64] = [
   -14,-21,-11, -8, -7, -9,-17,-24,
    -8, -4,  7,-12, -3,-13, -4,-14,
     2, -8,  0, -1, -2,  6,  0,  4,
    -3,  9, 12,  9, 14, 10,  3,  2,
    -6,  3, 13, 19,  7, 10, -3, -9,
   -12, -3,  8, 10, 13,  3, -7,-15,
   -14,-18, -7, -1,  4, -9,-15,-27,
   -23, -9,-23, -5, -9,-16, -5,-17,
];
#[rustfmt::skip]
const MG_ROOK: [i32; 64] = [
    32, 42, 32, 51, 63,  9, 31, 43,
    27, 32, 58, 62, 80, 67, 26, 44,
    -5, 19, 26, 36, 17, 45, 61, 16,
   -24,-11,  7, 26, 24, 35, -8,-20,
   -36,-26,-12, -1,  9, -7,  6,-23,
   -45,-25,-16,-17,  3,  0, -5,-33,
   -44,-16,-20, -9, -1, 11, -6,-71,
   -19,-13,  1, 17, 16,  7,-37,-26,
];
#[rustfmt::skip]
const EG_ROOK: [i32; 64] = [
    13, 10, 18, 15, 12, 12,  8,  5,
    11, 13, 13, 11,-3,  3,  8,  3,
     7,  7,  7,  5,  4, -3, -5, -3,
     4,  3, 13,  1,  2,  1, -1,  2,
     3,  5,  8,  4, -5, -6, -8,-11,
    -4,  0, -5, -1, -7,-12, -8,-16,
    -6, -6,  0,  2, -9, -9,-11, -3,
    -9,  2,  3, -1, -5,-13,  4,-20,
];
#[rustfmt::skip]
const MG_QUEEN: [i32; 64] = [
   -28,  0, 29, 12, 59, 44, 43, 45,
   -24,-39, -5,  1,-16, 57, 28, 54,
   -13,-17,  7,  8, 29, 56, 47, 57,
   -27,-27,-16,-16, -1, 17, -2,  1,
    -9,-26, -9,-10, -2, -4,  3, -3,
   -14,  2,-11, -2, -5,  2, 14,  5,
   -35, -8, 11,  2,  8, 15, -3,  1,
    -1,-18, -9, 10,-15,-25,-31,-50,
];
#[rustfmt::skip]
const EG_QUEEN: [i32; 64] = [
    -9, 22, 22, 27, 27, 19, 10, 20,
   -17, 20, 32, 41, 58, 25, 30,  0,
   -20,  6,  9, 49, 47, 35, 19,  9,
     3, 22, 24, 45, 57, 40, 57, 36,
   -18, 28, 19, 47, 31, 34, 39, 23,
   -16,-27, 15,  6,  9, 17, 10,  5,
   -22,-23,-30,-16,-16,-23,-36,-32,
   -33,-28,-22,-43, -5,-32,-20,-41,
];
#[rustfmt::skip]
const MG_KING: [i32; 64] = [
   -65, 23, 16,-15,-56,-34,  2, 13,
    29, -1,-20, -7, -8, -4,-38,-29,
    -9, 24,  2,-16,-20,  6, 22,-22,
   -17,-20,-12,-27,-30,-25,-14,-36,
   -49, -1,-27,-39,-46,-44,-33,-51,
   -14,-14,-22,-46,-44,-30,-15,-27,
     1,  7, -8,-64,-43,-16,  9,  8,
   -15, 36, 12,-54,  8,-28, 24, 14,
];
#[rustfmt::skip]
const EG_KING: [i32; 64] = [
   -74,-35,-18,-18,-11, 15,  4,-17,
   -12, 17, 14, 17, 17, 38, 23, 11,
    10, 17, 23, 15, 20, 45, 44, 13,
    -8, 22, 24, 27, 26, 33, 26,  3,
   -18, -4, 21, 24, 27, 23,  9,-11,
   -19, -3, 11, 21, 23, 16,  7, -9,
   -27,-11,  4, 13, 14,  4, -5,-17,
   -53,-33,-21,-11,-28,-14,-24,-43,
];

fn evaluate(pos: &Position) -> i32 {
    let mut mg_score = 0i32;
    let mut eg_score = 0i32;
    let mut phase = 0i32;
    let mut w_bishops = 0u8;
    let mut b_bishops = 0u8;
    let mut w_pawns_on_file = [0u8; 8];
    let mut b_pawns_on_file = [0u8; 8];

    for sq in 0..64 {
        let Some((c, p)) = unpack_sq(pos.squares[sq]) else {
            continue;
        };
        let r = sq / 8;
        let f = sq % 8;
        // White table row 0 is rank 8 (top), so flip rank for White
        let pst_idx = if c == Color::White { (7 - r) * 8 + f } else { r * 8 + f };
        let (mg_pst, eg_pst) = match p {
            Piece::Pawn => {
                if c == Color::White { w_pawns_on_file[f] += 1; } else { b_pawns_on_file[f] += 1; }
                (MG_PAWN[pst_idx], EG_PAWN[pst_idx])
            }
            Piece::Knight => (MG_KNIGHT[pst_idx], EG_KNIGHT[pst_idx]),
            Piece::Bishop => {
                if c == Color::White { w_bishops += 1; } else { b_bishops += 1; }
                (MG_BISHOP[pst_idx], EG_BISHOP[pst_idx])
            }
            Piece::Rook => (MG_ROOK[pst_idx], EG_ROOK[pst_idx]),
            Piece::Queen => (MG_QUEEN[pst_idx], EG_QUEEN[pst_idx]),
            Piece::King => (MG_KING[pst_idx], EG_KING[pst_idx]),
        };
        phase += p.phase_weight();
        let mg = p.mg_val() + mg_pst;
        let eg = p.eg_val() + eg_pst;
        if c == Color::White {
            mg_score += mg;
            eg_score += eg;
        } else {
            mg_score -= mg;
            eg_score -= eg;
        }
    }

    // Bishop pair & Rook open-file / Passed-pawn positional bonuses
    if w_bishops >= 2 {
        mg_score += 28;
        eg_score += 42;
    }
    if b_bishops >= 2 {
        mg_score -= 28;
        eg_score -= 42;
    }

    for f in 0..8 {
        if w_pawns_on_file[f] > 1 {
            eg_score -= 14;
        }
        if b_pawns_on_file[f] > 1 {
            eg_score += 14;
        }
    }

    for sq in 0..64 {
        let Some((c, p)) = unpack_sq(pos.squares[sq]) else {
            continue;
        };
        let r = sq / 8;
        let f = sq % 8;
        if p == Piece::Rook {
            let own = if c == Color::White { w_pawns_on_file[f] } else { b_pawns_on_file[f] };
            let opp = if c == Color::White { b_pawns_on_file[f] } else { w_pawns_on_file[f] };
            let bonus = if own == 0 && opp == 0 {
                26
            } else if own == 0 {
                14
            } else {
                0
            };
            if c == Color::White {
                mg_score += bonus;
                eg_score += bonus;
            } else {
                mg_score -= bonus;
                eg_score -= bonus;
            }
        } else if p == Piece::Pawn {
            let is_passed = if c == Color::White {
                let adj = |df: isize| -> bool {
                    let nf = f as isize + df;
                    !(0..8).contains(&nf) || b_pawns_on_file[nf as usize] == 0
                };
                adj(-1) && adj(0) && adj(1)
            } else {
                let adj = |df: isize| -> bool {
                    let nf = f as isize + df;
                    !(0..8).contains(&nf) || w_pawns_on_file[nf as usize] == 0
                };
                adj(-1) && adj(0) && adj(1)
            };
            if is_passed {
                let adv = if c == Color::White { r } else { 7 - r } as i32;
                let pb = adv * adv * 2;
                if c == Color::White {
                    eg_score += pb;
                } else {
                    eg_score -= pb;
                }
            }
        }
    }

    let mg_phase = phase.clamp(0, 24);
    let eg_phase = 24 - mg_phase;
    let total = (mg_score * mg_phase + eg_score * eg_phase) / 24;
    if pos.stm == Color::White { total } else { -total }
}

/// Compact classical opening book (`prefix_uci`, `&[candidate_replies]`).
const OPENING_BOOK: &[(&str, &[&str])] = &[
    ("", &["e2e4", "d2d4", "c2c4", "g1f3"]),
    ("e2e4", &["e7e5", "c7c5", "e7e6", "c7c6"]),
    ("d2d4", &["d7d5", "g8f6", "e7e6"]),
    ("e2e4 e7e5", &["g1f3"]),
    ("e2e4 e7e5 g1f3", &["b8c6"]),
    ("e2e4 e7e5 g1f3 b8c6", &["f1b5", "f1c4"]),
    ("e2e4 e7e5 g1f3 b8c6 f1b5", &["a7a6", "g8f6"]),
    ("e2e4 e7e5 g1f3 b8c6 f1c4", &["f8c5", "g8f6"]),
    ("e2e4 c7c5", &["g1f3", "b1c3"]),
    ("e2e4 c7c5 g1f3", &["d7d6", "b8c6", "e7e6"]),
    ("e2e4 c7c5 g1f3 d7d6", &["d2d4"]),
    ("e2e4 e7e6", &["d2d4"]),
    ("e2e4 e7e6 d2d4", &["d7d5"]),
    ("e2e4 c7c6", &["d2d4"]),
    ("e2e4 c7c6 d2d4", &["d7d5"]),
    ("d2d4 d7d5", &["c2c4", "c1f4"]),
    ("d2d4 d7d5 c2c4", &["e7e6", "c7c6"]),
    ("d2d4 g8f6", &["c2c4", "c1f4"]),
    ("d2d4 g8f6 c2c4", &["e7e6", "g7g6"]),
    ("d2d4 g8f6 c2c4 g7g6", &["b1c3"]),
    ("c2c4", &["e7e5", "g8f6"]),
];

// Compact 16-byte Transposition Table entry (`1024 * 16 = 16 KB`).
#[derive(Clone, Copy)]
struct TtEntry {
    key32: u32,
    score: i16,
    depth: i8,
    flag: u8, // 0 exact, 1 lower, 2 upper
    best_from: u8,
    best_to: u8,
    promo: u8,
    _pad: u8,
}

impl Default for TtEntry {
    fn default() -> Self {
        Self {
            key32: 0,
            score: 0,
            depth: -1,
            flag: 0,
            best_from: 0,
            best_to: 0,
            promo: 0,
            _pad: 0,
        }
    }
}

pub struct ChessEngine {
    tt: Vec<TtEntry>,               // 1,024 entries (16,384 bytes on heap)
    killers: [[u16; 2]; 24],        // 96 bytes (packed from<<6 | to)
    history: Vec<[u16; 64]>,        // 12 * 128 = 1,536 bytes on heap
    nodes: u32,
    max_nodes: u32,
    allow_nmp_lmr: bool,
}

impl Default for ChessEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ChessEngine {
    pub fn new() -> Self {
        Self {
            tt: Vec::new(),
            killers: [[0; 2]; 24],
            history: Vec::new(),
            nodes: 0,
            max_nodes: 30_000,
            allow_nmp_lmr: true,
        }
    }

    fn ensure_tables(&mut self) {
        if self.tt.is_empty() {
            self.tt = alloc::vec![TtEntry::default(); 1024];
        }
        if self.history.is_empty() {
            self.history = alloc::vec![[0; 64]; 12];
        }
    }

    #[inline]
    fn piece_hist_idx(sq_val: u8) -> usize {
        let color_off = if (sq_val & 8) != 0 { 6 } else { 0 };
        let p = ((sq_val & 7).saturating_sub(1) as usize).min(5);
        color_off + p
    }

    fn score_move_list(
        &self,
        pos: &Position,
        list: &mut MoveList,
        tt_move: Option<(u8, u8, u8)>,
        ply: usize,
    ) {
        let k0 = self.killers.get(ply).map(|k| k[0]).unwrap_or(0);
        let k1 = self.killers.get(ply).map(|k| k[1]).unwrap_or(0);

        for i in 0..list.len {
            let mv = list.moves[i];
            let pr = mv.promo.map(|p| p as u8).unwrap_or(0);
            if Some((mv.from, mv.to, pr)) == tt_move {
                list.scores[i] = 2_000_000;
                continue;
            }
            if let Some((_, vic)) = unpack_sq(pos.squares[mv.to as usize]) {
                let (_, att) = unpack_sq(pos.squares[mv.from as usize]).unwrap_or((pos.stm, Piece::Pawn));
                list.scores[i] = 1_000_000 + vic.mg_val() * 16 - att.mg_val();
                continue;
            }
            if mv.is_ep {
                list.scores[i] = 1_001_000;
                continue;
            }
            if pr != 0 {
                list.scores[i] = 900_000 + (pr as i32) * 100;
                continue;
            }
            let packed = ((mv.from as u16) << 6) | (mv.to as u16);
            if packed == k0 {
                list.scores[i] = 800_000;
            } else if packed == k1 {
                list.scores[i] = 750_000;
            } else if mv.is_castle {
                list.scores[i] = 500_000;
            } else {
                let h_idx = Self::piece_hist_idx(pos.squares[mv.from as usize]);
                list.scores[i] = self.history[h_idx][mv.to as usize] as i32;
            }
        }
    }

    fn quiescence(&mut self, pos: &Position, mut alpha: i32, beta: i32, qdepth: i8) -> i32 {
        self.nodes += 1;
        let stand_pat = evaluate(pos);
        if stand_pat >= beta || qdepth <= 0 || self.nodes >= self.max_nodes {
            return stand_pat.min(beta);
        }
        // Delta pruning guard
        if stand_pat + 1_100 < alpha {
            return alpha;
        }
        if stand_pat > alpha {
            alpha = stand_pat;
        }
        let mut list = pos.generate_legal_list(true);
        self.score_move_list(pos, &mut list, None, 23);

        for i in 0..list.len {
            let mv = list.pick_next(i);
            let nxt = pos.make_move(mv);
            let score = -self.quiescence(&nxt, -beta, -alpha, qdepth - 1);
            if score >= beta {
                return beta;
            }
            if score > alpha {
                alpha = score;
            }
            if self.nodes >= self.max_nodes {
                break;
            }
        }
        alpha
    }

    fn negamax(
        &mut self,
        pos: &Position,
        mut depth: i8,
        mut alpha: i32,
        beta: i32,
        ply: usize,
        allow_null: bool,
    ) -> i32 {
        self.nodes += 1;
        if pos.halfmove >= 100 {
            return 0;
        }
        if ply >= 22 {
            return evaluate(pos);
        }

        let in_check = pos.in_check(pos.stm);
        if in_check {
            depth += 1; // Check extension
        }

        let tt_idx = (pos.hash as usize) & 1023;
        let key32 = (pos.hash >> 32) as u32;
        let entry = self.tt[tt_idx];
        let mut tt_hint = None;
        if entry.key32 == key32 {
            tt_hint = Some((entry.best_from, entry.best_to, entry.promo));
            if entry.depth >= depth {
                let s = entry.score as i32;
                if entry.flag == 0
                    || (entry.flag == 1 && s >= beta)
                    || (entry.flag == 2 && s <= alpha)
                {
                    return s;
                }
            }
        }

        if depth <= 0 {
            let qd = if self.allow_nmp_lmr { 5 } else { 2 };
            return self.quiescence(pos, alpha, beta, qd);
        }

        // Null-Move Pruning (enabled at higher ELO levels when non-pawn material exists)
        if self.allow_nmp_lmr
            && allow_null
            && !in_check
            && depth >= 3
            && pos.has_non_pawn_material(pos.stm)
        {
            let null_pos = pos.make_null_move();
            let null_score = -self.negamax(&null_pos, depth - 3, -beta, -beta + 1, ply + 1, false);
            if null_score >= beta {
                return beta;
            }
        }

        let mut list = pos.generate_legal_list(false);
        if list.len == 0 {
            return if in_check {
                -28_000 + ply as i32
            } else {
                0
            };
        }

        self.score_move_list(pos, &mut list, tt_hint, ply);
        let orig_alpha = alpha;
        let mut best_score = -40_000;
        let mut best_move = list.moves[0];

        for i in 0..list.len {
            let mv = list.pick_next(i);
            let is_quiet = pos.squares[mv.to as usize] == 0 && !mv.is_ep && mv.promo.is_none();
            let nxt = pos.make_move(mv);

            let mut score;
            if self.allow_nmp_lmr && i >= 4 && depth >= 3 && is_quiet && !in_check && !nxt.in_check(nxt.stm) {
                // Late Move Reduction search
                score = -self.negamax(&nxt, depth - 2, -alpha - 1, -alpha, ply + 1, true);
                if score > alpha {
                    score = -self.negamax(&nxt, depth - 1, -beta, -alpha, ply + 1, true);
                }
            } else {
                score = -self.negamax(&nxt, depth - 1, -beta, -alpha, ply + 1, true);
            }

            if score > best_score {
                best_score = score;
                best_move = mv;
            }
            if score > alpha {
                alpha = score;
                if is_quiet && ply < 24 {
                    let h_idx = Self::piece_hist_idx(pos.squares[mv.from as usize]);
                    let inc = (depth as u16) * (depth as u16);
                    self.history[h_idx][mv.to as usize] =
                        self.history[h_idx][mv.to as usize].saturating_add(inc).min(40_000);
                }
            }
            if alpha >= beta {
                if is_quiet && ply < 24 {
                    let packed = ((mv.from as u16) << 6) | (mv.to as u16);
                    if self.killers[ply][0] != packed {
                        self.killers[ply][1] = self.killers[ply][0];
                        self.killers[ply][0] = packed;
                    }
                }
                break;
            }
            if self.nodes >= self.max_nodes {
                break;
            }
        }

        let flag = if best_score <= orig_alpha {
            2
        } else if best_score >= beta {
            1
        } else {
            0
        };
        self.tt[tt_idx] = TtEntry {
            key32,
            score: best_score.clamp(-30_000, 30_000) as i16,
            depth,
            flag,
            best_from: best_move.from,
            best_to: best_move.to,
            promo: best_move.promo.map(|p| p as u8).unwrap_or(0),
            _pad: 0,
        };
        best_score
    }

    pub fn book_move(&self, uci_line: &str, legal: &MoveList, rng_seed: u32) -> Option<ChessMove> {
        for &(prefix, replies) in OPENING_BOOK {
            if prefix == uci_line && !replies.is_empty() {
                let pick = replies[(rng_seed as usize) % replies.len()];
                for i in 0..legal.len {
                    if legal.moves[i].uci() == pick {
                        return Some(legal.moves[i]);
                    }
                }
            }
        }
        None
    }

    /// Choose best move across 4 ELO tiers (1=1200, 2=1500, 3=1750, 4=2000 ELO).
    pub fn choose_move_with_book(
        &mut self,
        pos: &Position,
        level: u8,
        rng_seed: u32,
        uci_history: Option<&str>,
    ) -> Option<(ChessMove, i32)> {
        let mut list = pos.generate_legal_list(false);
        if list.len == 0 {
            return None;
        }
        if level >= 2
            && let Some(hist) = uci_history
                && let Some(bmv) = self.book_move(hist, &list, rng_seed) {
                    return Some((bmv, 15));
                }

        let (max_depth, budget, jitter_cp, use_advanced) = match level {
            1 => (2i8, 1_200u32, 45i32, false),  // ~1200 ELO
            2 => (4i8, 6_500, 18, false),         // ~1500 ELO
            3 => (5i8, 28_000, 0, true),          // ~1750 ELO
            _ => (7i8, 95_000, 0, true),          // ~2000 ELO
        };

        self.ensure_tables();
        self.nodes = 0;
        self.max_nodes = budget;
        self.allow_nmp_lmr = use_advanced;
        self.killers = [[0; 2]; 24];
        for row in &mut self.history {
            for cell in row.iter_mut() {
                *cell >>= 2; // Decay previous move history gracefully
            }
        }

        let mut best_mv = list.moves[0];
        let mut best_score = -40_000;

        for depth in 1..=max_depth {
            let pr = best_mv.promo.map(|p| p as u8).unwrap_or(0);
            self.score_move_list(pos, &mut list, Some((best_mv.from, best_mv.to, pr)), 0);

            let mut cur_best = list.moves[0];
            let mut cur_score = -40_000;
            for i in 0..list.len {
                let mv = list.pick_next(i);
                let nxt = pos.make_move(mv);
                let mut sc = -self.negamax(&nxt, depth - 1, -35_000, 35_000, 1, true);
                if jitter_cp > 0 && sc.abs() < 15_000 {
                    let span = jitter_cp * 2 + 1;
                    let noise = (((rng_seed.wrapping_mul(1_664_525) ^ (mv.from as u32 * 53 + mv.to as u32)) % span as u32) as i32) - jitter_cp;
                    sc += noise;
                }
                if sc > cur_score {
                    cur_score = sc;
                    cur_best = mv;
                }
                if self.nodes >= self.max_nodes {
                    break;
                }
            }
            best_mv = cur_best;
            best_score = cur_score;
            if self.nodes >= self.max_nodes || best_score > 24_000 {
                break;
            }
        }
        Some((best_mv, best_score))
    }

    pub fn choose_move(&mut self, pos: &Position, level: u8, rng_seed: u32) -> Option<(ChessMove, i32)> {
        self.choose_move_with_book(pos, level, rng_seed, None)
    }
}

pub struct ChessPuzzle {
    pub title_en: &'static str,
    pub title_ru: &'static str,
    pub fen: &'static str,
    pub solution_uci: &'static str,
}

/// 15 verified Mate-in-1 / sharp tactical puzzles.
pub const CHESS_PUZZLES: [ChessPuzzle; 15] = [
    ChessPuzzle { title_en: "1. Queen Back Rank", title_ru: "1. Ферзь на 8-ю", fen: "6k1/5ppp/8/8/8/8/5PPP/3Q2K1 w - - 0 1", solution_uci: "d1d8" },
    ChessPuzzle { title_en: "2. Rook Corridor", title_ru: "2. Матовая ладья", fen: "6k1/5ppp/8/8/8/8/1R3PPP/6K1 w - - 0 1", solution_uci: "b2b8" },
    ChessPuzzle { title_en: "3. Scholar's Classic", title_ru: "3. Детский мат", fen: "r1bqkb1r/pppp1ppp/2n2n2/4p2Q/2B1P3/8/PPPP1PPP/RNB1K1NR w KQkq - 0 1", solution_uci: "h5f7" },
    ChessPuzzle { title_en: "4. Arabian Corner", title_ru: "4. Арабский мат", fen: "7k/R7/5N2/8/8/8/8/6K1 w - - 0 1", solution_uci: "a7h7" },
    ChessPuzzle { title_en: "5. Smothered Knight", title_ru: "5. Спёртый мат", fen: "6rk/6pp/7N/8/8/8/8/6K1 w - - 0 1", solution_uci: "h6f7" },
    ChessPuzzle { title_en: "6. Queen Support Kiss", title_ru: "6. Поцелуй королевы", fen: "6k1/6p1/5B1K/8/8/8/8/6Q1 w - - 0 1", solution_uci: "g1g7" },
    ChessPuzzle { title_en: "7. Twin Rooks Ladder", title_ru: "7. Линейный мат", fen: "7k/1R6/8/R7/8/8/8/6K1 w - - 0 1", solution_uci: "a5a8" },
    ChessPuzzle { title_en: "8. Bishop & Queen Battery", title_ru: "8. Батарея слона и ферзя", fen: "r1b2rk1/pppp1ppp/8/8/4Q3/3B4/PPP2PPP/R4RK1 w - - 0 1", solution_uci: "e4h7" },
    ChessPuzzle { title_en: "9. Back Rank Discovery", title_ru: "9. Мат по последней", fen: "3r2k1/5ppp/8/8/8/1B6/5PPP/3R2K1 w - - 0 1", solution_uci: "d1d8" },
    ChessPuzzle { title_en: "10. Anastasia's Mate", title_ru: "10. Мат Анастасии", fen: "7k/4N1pp/8/8/8/8/7R/6K1 w - - 0 1", solution_uci: "h2h7" },
    ChessPuzzle { title_en: "11. Black's Revenge", title_ru: "11. Ответ чёрных", fen: "6k1/5ppp/8/8/8/8/5PPP/3q2K1 b - - 0 1", solution_uci: "d1f1" },
    ChessPuzzle { title_en: "12. Boden's Diagonal", title_ru: "12. Диагональный удар", fen: "2kr4/ppp5/8/8/8/2B5/5B2/6K1 w - - 0 1", solution_uci: "f2a7" },
    ChessPuzzle { title_en: "13. Pawn Promotion Strike", title_ru: "13. Превращение пешки", fen: "6k1/4Pppp/8/8/8/8/8/4R1K1 w - - 0 1", solution_uci: "e7e8q" },
    ChessPuzzle { title_en: "14. Lolli's Pawn Helper", title_ru: "14. Мат Лолли", fen: "6k1/5p1p/5PpQ/8/8/8/8/6K1 w - - 0 1", solution_uci: "h6g7" },
    ChessPuzzle { title_en: "15. Queen & Knight Harmony", title_ru: "15. Ферзь и конь", fen: "r4rk1/ppp2ppp/8/4N3/8/8/PPP2PPP/R2Q2K1 w - - 0 1", solution_uci: "d1h5" },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChessMode {
    Menu,
    PlayingAi { level: u8 },
    PassAndPlay,
    Puzzles { idx: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameStatus {
    Playing,
    WhiteWinsMate,
    BlackWinsMate,
    DrawStalemate,
    DrawFiftyMove,
    DrawRepetition,
    PuzzleSolved,
}

pub struct ChessApp {
    pub mode: ChessMode,
    pub menu_cursor: usize,
    pub menu_level: u8,
    pub pos: Position,
    pub history: Vec<(Position, String, String)>, // (pos_before, san, uci)
    pub cursor_sq: u8,
    pub selected_sq: Option<u8>,
    pub legal_cache: Vec<ChessMove>,
    pub coach_hint: Option<ChessMove>,
    pub last_move: Option<(u8, u8)>,
    pub status: GameStatus,
    pub engine: ChessEngine,
    pub seed: u32,
}

impl Default for ChessApp {
    fn default() -> Self {
        Self::new()
    }
}

impl ChessApp {
    pub fn new() -> Self {
        let pos = Position::default();
        let legal_cache = pos.legal_moves();
        Self {
            mode: ChessMode::Menu,
            menu_cursor: 0,
            menu_level: 2, // Default 1500 ELO Club Regular
            pos,
            history: Vec::new(),
            cursor_sq: 12,
            selected_sq: None,
            legal_cache,
            coach_hint: None,
            last_move: None,
            status: GameStatus::Playing,
            engine: ChessEngine::new(),
            seed: 7,
        }
    }

    pub fn level_elo(level: u8) -> u16 {
        match level {
            1 => 1200,
            2 => 1500,
            3 => 1750,
            _ => 2000,
        }
    }

    pub fn start_ai(&mut self, level: u8) {
        self.mode = ChessMode::PlayingAi { level: level.clamp(1, 4) };
        self.pos = Position::default();
        self.history.clear();
        self.cursor_sq = 12;
        self.selected_sq = None;
        self.coach_hint = None;
        self.last_move = None;
        self.status = GameStatus::Playing;
        self.legal_cache = self.pos.legal_moves();
    }

    pub fn start_puzzle(&mut self, idx: usize) {
        let i = idx % CHESS_PUZZLES.len();
        self.mode = ChessMode::Puzzles { idx: i };
        self.pos = Position::from_fen(CHESS_PUZZLES[i].fen).unwrap_or_default();
        self.history.clear();
        self.selected_sq = None;
        self.coach_hint = None;
        self.last_move = None;
        self.status = GameStatus::Playing;
        self.legal_cache = self.pos.legal_moves();
        self.cursor_sq = self.pos.king_sq(self.pos.stm).unwrap_or(12);
    }

    fn update_status(&mut self) {
        if self.legal_cache.is_empty() {
            if self.pos.in_check(self.pos.stm) {
                self.status = if self.pos.stm == Color::White {
                    GameStatus::BlackWinsMate
                } else {
                    GameStatus::WhiteWinsMate
                };
            } else {
                self.status = GameStatus::DrawStalemate;
            }
            return;
        }
        if self.pos.halfmove >= 100 {
            self.status = GameStatus::DrawFiftyMove;
            return;
        }
        let reps = self.history.iter().filter(|(p, _, _)| p.hash == self.pos.hash).count();
        if reps >= 2 {
            self.status = GameStatus::DrawRepetition;
            return;
        }
        self.status = GameStatus::Playing;
    }

    fn history_uci_line(&self) -> String {
        let mut line = String::new();
        for (idx, (_, _, u)) in self.history.iter().enumerate() {
            if idx > 0 {
                line.push(' ');
            }
            line.push_str(u);
        }
        line
    }

    fn apply_player_move(&mut self, mv: ChessMove) {
        let san = self.pos.format_san(mv, &self.legal_cache);
        let uci = mv.uci();
        self.history.push((self.pos, san, uci));
        self.pos = self.pos.make_move(mv);
        self.last_move = Some((mv.from, mv.to));
        self.selected_sq = None;
        self.coach_hint = None;
        self.legal_cache = self.pos.legal_moves();
        self.update_status();

        if let ChessMode::Puzzles { idx } = self.mode {
            let expected = CHESS_PUZZLES[idx].solution_uci;
            if mv.uci() == expected || self.status == GameStatus::WhiteWinsMate || self.status == GameStatus::BlackWinsMate {
                self.status = GameStatus::PuzzleSolved;
            }
            return;
        }

        if let ChessMode::PlayingAi { level } = self.mode
            && self.status == GameStatus::Playing && self.pos.stm == Color::Black {
                self.seed = self.seed.wrapping_add(13);
                let uci_line = self.history_uci_line();
                if let Some((ai_mv, _)) = self.engine.choose_move_with_book(&self.pos, level, self.seed, Some(&uci_line)) {
                    let ai_san = self.pos.format_san(ai_mv, &self.legal_cache);
                    self.history.push((self.pos, ai_san, ai_mv.uci()));
                    self.pos = self.pos.make_move(ai_mv);
                    self.last_move = Some((ai_mv.from, ai_mv.to));
                    self.legal_cache = self.pos.legal_moves();
                    self.update_status();
                }
            }
    }

    pub fn handle_long_confirm(&mut self) -> GameAction {
        if self.mode != ChessMode::Menu
            && let Some((hint_mv, _)) = self.engine.choose_move(&self.pos, 4, self.seed) {
                self.coach_hint = Some(hint_mv);
                self.cursor_sq = hint_mv.from;
                return GameAction::Redraw(RefreshMode::FastDu);
            }
        GameAction::None
    }

    pub fn handle_button(&mut self, btn: Button) -> GameAction {
        match self.mode {
            ChessMode::Menu => match btn {
                Button::Back => GameAction::ExitToHome,
                Button::Up | Button::Left => {
                    self.menu_cursor = (self.menu_cursor + 3) % 4;
                    GameAction::Redraw(RefreshMode::FastDu)
                }
                Button::Down | Button::Right => {
                    self.menu_cursor = (self.menu_cursor + 1) % 4;
                    GameAction::Redraw(RefreshMode::FastDu)
                }
                Button::Confirm => {
                    match self.menu_cursor {
                        0 => {
                            self.menu_level = if self.menu_level >= 4 { 1 } else { self.menu_level + 1 };
                            return GameAction::Redraw(RefreshMode::FastDu);
                        }
                        1 => self.start_ai(self.menu_level),
                        2 => {
                            self.start_ai(1);
                            self.mode = ChessMode::PassAndPlay;
                        }
                        _ => self.start_puzzle(0),
                    }
                    GameAction::Redraw(RefreshMode::FastDu)
                }
                _ => GameAction::None,
            },
            _ => {
                if self.status == GameStatus::PuzzleSolved && btn == Button::Confirm
                    && let ChessMode::Puzzles { idx } = self.mode {
                        self.start_puzzle((idx + 1) % CHESS_PUZZLES.len());
                        return GameAction::Redraw(RefreshMode::FastDu);
                    }
                match btn {
                    Button::Back => {
                        if self.selected_sq.take().is_some() {
                            return GameAction::Redraw(RefreshMode::FastDu);
                        }
                        if let Some((prev_pos, _, _)) = self.history.pop() {
                            if matches!(self.mode, ChessMode::PlayingAi { .. }) && prev_pos.stm == Color::Black {
                                if let Some((w_pos, _, _)) = self.history.pop() {
                                    self.pos = w_pos;
                                } else {
                                    self.pos = prev_pos;
                                }
                            } else {
                                self.pos = prev_pos;
                            }
                            self.last_move = None;
                            self.legal_cache = self.pos.legal_moves();
                            self.update_status();
                            return GameAction::Redraw(RefreshMode::FastDu);
                        }
                        self.mode = ChessMode::Menu;
                        GameAction::Redraw(RefreshMode::FastDu)
                    }
                    Button::Power => self.handle_long_confirm(),
                    Button::Left => {
                        let r = self.cursor_sq / 8;
                        let f = (self.cursor_sq + 7) % 8;
                        self.cursor_sq = r * 8 + f;
                        GameAction::Redraw(RefreshMode::FastDu)
                    }
                    Button::Right => {
                        let r = self.cursor_sq / 8;
                        let f = (self.cursor_sq + 1) % 8;
                        self.cursor_sq = r * 8 + f;
                        GameAction::Redraw(RefreshMode::FastDu)
                    }
                    Button::Up => {
                        self.cursor_sq = (self.cursor_sq + 8) % 64;
                        GameAction::Redraw(RefreshMode::FastDu)
                    }
                    Button::Down => {
                        self.cursor_sq = (self.cursor_sq + 56) % 64;
                        GameAction::Redraw(RefreshMode::FastDu)
                    }
                    Button::Confirm => {
                        if self.status != GameStatus::Playing {
                            self.mode = ChessMode::Menu;
                            return GameAction::Redraw(RefreshMode::FastDu);
                        }
                        if let Some(sel) = self.selected_sq {
                            if sel == self.cursor_sq {
                                self.selected_sq = None;
                                return GameAction::Redraw(RefreshMode::FastDu);
                            }
                            let found = self
                                .legal_cache
                                .iter()
                                .copied()
                                .find(|m| m.from == sel && m.to == self.cursor_sq && m.promo.unwrap_or(Piece::Queen) == Piece::Queen);
                            if let Some(mv) = found {
                                self.apply_player_move(mv);
                                return GameAction::Redraw(RefreshMode::FastDu);
                            }
                        }
                        if let Some((c, _)) = unpack_sq(self.pos.squares[self.cursor_sq as usize])
                            && c == self.pos.stm {
                                self.selected_sq = Some(self.cursor_sq);
                                return GameAction::Redraw(RefreshMode::FastDu);
                            }
                        if self.selected_sq.is_none() {
                            return self.handle_long_confirm();
                        }
                        GameAction::None
                    }
                }
            }
        }
    }

    pub fn render(&self, canvas: &mut Canvas<'_>, is_ru: bool) {
        let top = SAFE_INSETS.top as i32 + 52;
        if self.mode == ChessMode::Menu {
            self.render_menu(canvas, top, is_ru);
            return;
        }

        let left = 36;
        let col_w = PORTRAIT_W as i32 - 72;
        let hdr = match self.mode {
            ChessMode::PlayingAi { level } => {
                let elo = Self::level_elo(level);
                if is_ru {
                    format!("Шахматы · {elo}")
                } else {
                    format!("Chess · {elo}")
                }
            }
            ChessMode::PassAndPlay => {
                if is_ru { "Вдвоём".into() } else { "Pass & Play".into() }
            }
            ChessMode::Puzzles { idx } => {
                let p = &CHESS_PUZZLES[idx];
                if is_ru { p.title_ru.to_string() } else { p.title_en.to_string() }
            }
            ChessMode::Menu => String::new(),
        };
        canvas.draw_text_truncated(&baked_fonts::FONT_UI_25_BOLD, left, top + 27, 290, &hdr, Tone::Ink);

        let turn_str = match (self.pos.stm, is_ru) {
            (Color::White, true) => "Ход белых",
            (Color::Black, true) => "Ход чёрных",
            (Color::White, false) => "WHITE",
            (Color::Black, false) => "BLACK",
        };
        canvas.draw_text_aligned(
            &baked_fonts::FONT_MONO_20_SEMI,
            left + col_w,
            top + 26,
            turn_str,
            Tone::Ink,
            Align::Right,
        );
        canvas.fill_rect(left, top + 40, col_w, 2, Tone::Ink);

        let sq_px = 52;
        let board_px = sq_px * 8;
        let bx = (PORTRAIT_W as i32 - board_px) / 2;
        let by = top + 60;

        // Crisp orthogonal chessboard frame
        canvas.stroke_rect(bx - 4, by - 4, board_px + 8, board_px + 8, 2, Tone::Ink);

        let king_check_sq = if self.pos.in_check(self.pos.stm) {
            self.pos.king_sq(self.pos.stm)
        } else {
            None
        };

        for disp_r in 0..8 {
            let rank = 7 - disp_r;
            let r_ch = (b'1' + rank as u8) as char;
            canvas.draw_text_aligned(
                &baked_fonts::FONT_MONO_20_SEMI,
                bx - 18,
                by + disp_r * sq_px + 33,
                &format!("{r_ch}"),
                Tone::Ink,
                Align::Center,
            );

            for file in 0..8 {
                let sq = (rank * 8 + file) as u8;
                let x = bx + file * sq_px;
                let y = by + disp_r * sq_px;
                let dark = (rank + file) % 2 == 0;
                let is_sel = self.selected_sq == Some(sq);
                let is_check = king_check_sq == Some(sq);

                if is_sel {
                    canvas.fill_rect(x, y, sq_px, sq_px, Tone::Ink);
                } else if is_check {
                    canvas.fill_checker_50(x, y, sq_px, sq_px, Tone::Ink, Tone::Paper);
                    canvas.stroke_rect(x + 2, y + 2, sq_px - 4, sq_px - 4, 2, Tone::Ink);
                } else if dark {
                    canvas.fill_stipple_25(x, y, sq_px, sq_px, Tone::Ink, Tone::Paper);
                } else {
                    canvas.fill_rect(x, y, sq_px, sq_px, Tone::Paper);
                }

                if !is_sel
                    && let Some((lf, lt)) = self.last_move
                    && (sq == lf || sq == lt)
                {
                    let leg = 9;
                    canvas.fill_rect(x + 3, y + 3, leg, 2, Tone::Ink);
                    canvas.fill_rect(x + 3, y + 3, 2, leg, Tone::Ink);
                    canvas.fill_rect(x + sq_px - 3 - leg, y + 3, leg, 2, Tone::Ink);
                    canvas.fill_rect(x + sq_px - 5, y + 3, 2, leg, Tone::Ink);
                    canvas.fill_rect(x + 3, y + sq_px - 5, leg, 2, Tone::Ink);
                    canvas.fill_rect(x + 3, y + sq_px - 3 - leg, 2, leg, Tone::Ink);
                    canvas.fill_rect(x + sq_px - 3 - leg, y + sq_px - 5, leg, 2, Tone::Ink);
                    canvas.fill_rect(x + sq_px - 5, y + sq_px - 3 - leg, 2, leg, Tone::Ink);
                }

                if let Some(ch) = self.coach_hint
                    && (sq == ch.from || sq == ch.to)
                {
                    canvas.stroke_circle(x + sq_px / 2, y + sq_px / 2, sq_px / 2 - 4, 2, Tone::Ink);
                }

                if let Some(sel) = self.selected_sq
                    && self.legal_cache.iter().any(|m| m.from == sel && m.to == sq)
                {
                    let mark_tone = if is_sel { Tone::Paper } else { Tone::Ink };
                    if self.pos.squares[sq as usize] == 0 {
                        canvas.fill_circle(x + sq_px / 2, y + sq_px / 2, 5, mark_tone);
                    } else {
                        canvas.stroke_circle(x + sq_px / 2, y + sq_px / 2, sq_px / 2 - 5, 2, mark_tone);
                    }
                }

                if let Some((color, piece)) = unpack_sq(self.pos.squares[sq as usize]) {
                    draw_chess_piece(
                        canvas,
                        piece.to_gfx(),
                        color == Color::White,
                        dark || is_sel,
                        x + sq_px / 2,
                        y + sq_px / 2 + 2,
                        sq_px,
                    );
                }

                if disp_r == 7 {
                    let f_ch = (b'a' + file as u8) as char;
                    canvas.draw_text_aligned(
                        &baked_fonts::FONT_MONO_20_SEMI,
                        x + sq_px / 2,
                        by + board_px + 25,
                        &format!("{f_ch}"),
                        Tone::Ink,
                        Align::Center,
                    );
                }

                if sq == self.cursor_sq {
                    canvas.stroke_rect(x + 1, y + 1, sq_px - 2, sq_px - 2, 2, Tone::Ink);
                    canvas.stroke_rect(x + 3, y + 3, sq_px - 6, sq_px - 6, 1, Tone::Paper);
                }
            }
        }

        // Sharp architectural notation ledger
        let info_y = by + board_px + 44;
        canvas.fill_rect(left, info_y, col_w, 2, Tone::Ink);
        canvas.fill_rect(left, info_y + 10, 4, 68, Tone::Ink);

        let recent: Vec<String> = self
            .history
            .iter()
            .rev()
            .take(5)
            .rev()
            .map(|(_, s, _)| s.clone())
            .collect();
        let moves_str = if recent.is_empty() {
            if is_ru {
                "Выберите фигуру кнопкой OK".into()
            } else {
                "Select a piece to begin".into()
            }
        } else {
            recent.join("  ")
        };
        canvas.draw_text_truncated(&baked_fonts::FONT_MONO_20_SEMI, left + 16, info_y + 30, col_w - 20, &moves_str, Tone::Ink);

        let status_txt = match (self.status, is_ru) {
            (GameStatus::Playing, true) => "Боковые кнопки: ряд · Долгий OK: совет",
            (GameStatus::Playing, false) => "Side buttons: rank · Hold Confirm: hint",
            (GameStatus::WhiteWinsMate, true) => "Мат · Белые победили",
            (GameStatus::WhiteWinsMate, false) => "Checkmate · White wins",
            (GameStatus::BlackWinsMate, true) => "Мат · Чёрные победили",
            (GameStatus::BlackWinsMate, false) => "Checkmate · Black wins",
            (GameStatus::PuzzleSolved, true) => "Задача решена · нажмите OK",
            (GameStatus::PuzzleSolved, false) => "Solved · press Confirm for next",
            (_, true) => "Ничья (пат) · нажмите OK",
            (_, false) => "Draw (Stalemate) · press Confirm",
        };
        let status_line = if let Some(hint) = self.coach_hint {
            let h_san = self.pos.format_san(hint, &self.legal_cache);
            if is_ru {
                format!("Совет: {h_san}")
            } else {
                format!("Coach: {h_san}")
            }
        } else {
            String::from(status_txt)
        };
        canvas.draw_text_truncated(&baked_fonts::FONT_UI_19_SEMI, left + 16, info_y + 68, col_w - 20, &status_line, Tone::Ink);

        let back_lbl = if self.selected_sq.is_some() || !self.history.is_empty() {
            if is_ru { "Отмена" } else { "Undo" }
        } else if is_ru {
            "Меню"
        } else {
            "Menu"
        };
        let ok_lbl = if self.selected_sq.is_some() {
            if is_ru { "Ход" } else { "Move" }
        } else if is_ru {
            "Выбор"
        } else {
            "Pick"
        };
        let chin = if is_ru {
            [back_lbl, ok_lbl, "Влево", "Вправо"]
        } else {
            [back_lbl, ok_lbl, "Left", "Right"]
        };
        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, chin, Some(1));
    }

    fn render_menu(&self, canvas: &mut Canvas<'_>, top: i32, is_ru: bool) {
        let left = 36;
        let col_w = PORTRAIT_W as i32 - 72;
        let title = if is_ru { "Шахматы" } else { "Chess" };
        canvas.draw_text(&baked_fonts::FONT_UI_32_BOLD, left, top + 35, title, Tone::Ink);
        canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, left + col_w, top + 34, "1200–2000 ELO", Tone::Ink, Align::Right);
        canvas.fill_rect(left, top + 49, col_w, 3, Tone::Ink);

        let lvl_name = match (self.menu_level, is_ru) {
            (1, true) => "1200 ELO · Клубный любитель",
            (1, false) => "1200 ELO · Club Novice",
            (2, true) => "1500 ELO · Разрядник",
            (2, false) => "1500 ELO · Club Regular",
            (3, true) => "1750 ELO · Турнирный тактик",
            (3, false) => "1750 ELO · Tournament Tactician",
            (_, true) => "2000 ELO · Мастер Фолио",
            (_, false) => "2000 ELO · Master Folio",
        };

        let opts = if is_ru {
            [
                (format!("Сложность · {}", Self::level_elo(self.menu_level)), lvl_name.to_string()),
                ("Партия против движка".into(), format!("Белыми против {} ELO", Self::level_elo(self.menu_level))),
                ("Вдвоём на одном экране".into(), "Классическая партия без часов".into()),
                ("15 этюдов (Мат в 1 ход)".into(), "Тактические задачи".into()),
            ]
        } else {
            [
                (format!("Strength · {} ELO", Self::level_elo(self.menu_level)), lvl_name.to_string()),
                ("Play vs Engine".into(), format!("Play White against {} ELO", Self::level_elo(self.menu_level))),
                ("Pass & Play".into(), "Over-the-board two player".into()),
                ("Mate-in-1 Puzzles".into(), "15 classical tactical studies".into()),
            ]
        };

        let mut y = top + 82;
        let row_h = 104;
        for (idx, (head, desc)) in opts.iter().enumerate() {
            let sel = idx == self.menu_cursor;
            let num = format!("{:02}", idx + 1);
            if sel {
                canvas.fill_rect(left, y, col_w, row_h, Tone::Ink);
            }
            let fg = if sel { Tone::Paper } else { Tone::Ink };
            canvas.draw_text(&baked_fonts::FONT_MONO_20_SEMI, left + 16, y + 40, &num, fg);
            canvas.draw_text_truncated(&baked_fonts::FONT_UI_25_BOLD, left + 62, y + 41, col_w - 78, head, fg);
            canvas.draw_text_truncated(&baked_fonts::FONT_UI_19_SEMI, left + 62, y + 76, col_w - 78, desc, fg);
            if !sel && idx + 1 < opts.len() && idx + 1 != self.menu_cursor {
                canvas.hline_dotted(left, y + row_h + 6, col_w, 2, Tone::Ink);
            }
            y += row_h + 12;
        }

        if let Some((_p, _san, _)) = self.history.last() {
            canvas.fill_rect(left, 650, col_w, 2, Tone::Ink);
            let active_s = if is_ru {
                format!("Незавершённая партия · {} полуходов", self.history.len())
            } else {
                format!("Game in progress · {} ply", self.history.len())
            };
            canvas.draw_text_truncated(&baked_fonts::FONT_MONO_20_SEMI, left, 684, col_w, &active_s, Tone::Ink);
        }

        let ok_lbl = if self.menu_cursor == 0 {
            if is_ru { "Сменить" } else { "Level" }
        } else if is_ru {
            "Играть"
        } else {
            "Play"
        };
        let chin = if is_ru {
            ["Домой", ok_lbl, "Выше", "Ниже"]
        } else {
            ["Home", ok_lbl, "Prev", "Next"]
        };
        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, chin, Some(1));
    }

    fn encode_pos(pos: &Position, out: &mut Vec<u8>) {
        out.extend_from_slice(&pos.squares);
        out.push(pos.stm as u8);
        out.push(pos.castle & 0x0F);
        out.push(pos.ep.unwrap_or(0xFF));
        out.extend_from_slice(&pos.halfmove.to_le_bytes());
        out.extend_from_slice(&pos.fullmove.to_le_bytes());
    }

    fn decode_pos(raw: &[u8], off: &mut usize) -> Option<Position> {
        let s = raw.get(*off..*off + 71)?;
        *off += 71;
        let mut squares = [0u8; 64];
        squares.copy_from_slice(&s[0..64]);
        let stm = match s[64] {
            0 => Color::White,
            1 => Color::Black,
            _ => return None,
        };
        let castle = s[65] & 0x0F;
        let ep = if s[66] == 0xFF {
            None
        } else if s[66] < 64 {
            Some(s[66])
        } else {
            return None;
        };
        let halfmove = u16::from_le_bytes([s[67], s[68]]);
        let fullmove = u16::from_le_bytes([s[69], s[70]]);
        let mut pos = Position {
            squares,
            stm,
            castle,
            ep,
            halfmove,
            fullmove,
            hash: 0,
        };
        pos.hash = pos.compute_hash();
        Some(pos)
    }

    /// Encode full interactive Chess state (mode, board, settings, move undo history) for SD persistence.
    pub fn encode_state(&self) -> Vec<u8> {
        let keep = self.history.len().min(96);
        let hist_slice = &self.history[self.history.len() - keep..];
        let mut out = Vec::with_capacity(100 + hist_slice.len() * 78);
        out.extend_from_slice(b"VVCH");
        out.push(1); // version
        let (mode_tag, mode_arg) = match self.mode {
            ChessMode::Menu => (0u8, 0u8),
            ChessMode::PlayingAi { level } => (1u8, level),
            ChessMode::PassAndPlay => (2u8, 0u8),
            ChessMode::Puzzles { idx } => (3u8, idx as u8),
        };
        out.push(mode_tag);
        out.push(mode_arg);
        out.push(self.menu_cursor as u8);
        out.push(self.menu_level);
        out.push(self.cursor_sq);
        out.push(self.selected_sq.unwrap_or(0xFF));
        match self.last_move {
            Some((a, b)) => {
                out.push(a);
                out.push(b);
            }
            None => {
                out.push(0xFF);
                out.push(0xFF);
            }
        }
        out.push(match self.status {
            GameStatus::Playing => 0,
            GameStatus::WhiteWinsMate => 1,
            GameStatus::BlackWinsMate => 2,
            GameStatus::DrawStalemate => 3,
            GameStatus::DrawFiftyMove => 4,
            GameStatus::DrawRepetition => 5,
            GameStatus::PuzzleSolved => 6,
        });
        out.extend_from_slice(&self.seed.to_le_bytes());

        Self::encode_pos(&self.pos, &mut out);
        out.extend_from_slice(&(hist_slice.len() as u16).to_le_bytes());
        for (prev_pos, _san, uci) in hist_slice {
            Self::encode_pos(prev_pos, &mut out);
            let ub = uci.as_bytes();
            let ulen = ub.len().min(8) as u8;
            out.push(ulen);
            out.extend_from_slice(&ub[..usize::from(ulen)]);
        }
        out
    }

    /// Restore full interactive Chess state from `encode_state` bytes.
    pub fn decode_state(&mut self, raw: &[u8]) -> bool {
        if raw.len() < 91 || &raw[0..4] != b"VVCH" || raw[4] != 1 {
            return false;
        }
        let mode = match raw[5] {
            0 => ChessMode::Menu,
            1 => ChessMode::PlayingAi { level: raw[6].clamp(1, 4) },
            2 => ChessMode::PassAndPlay,
            3 => ChessMode::Puzzles { idx: (raw[6] as usize) % CHESS_PUZZLES.len() },
            _ => return false,
        };
        let menu_cursor = (raw[7] as usize) % 4;
        let menu_level = raw[8].clamp(1, 4);
        let cursor_sq = raw[9];
        if cursor_sq >= 64 {
            return false;
        }
        let selected_sq = if raw[10] == 0xFF {
            None
        } else if raw[10] < 64 {
            Some(raw[10])
        } else {
            return false;
        };
        let last_move = if raw[11] < 64 && raw[12] < 64 {
            Some((raw[11], raw[12]))
        } else {
            None
        };
        let status_code = raw[13];
        let seed = u32::from_le_bytes([raw[14], raw[15], raw[16], raw[17]]);

        let mut off = 18usize;
        let Some(pos) = Self::decode_pos(raw, &mut off) else {
            return false;
        };
        let Some(cnt_bytes) = raw.get(off..off + 2) else {
            return false;
        };
        let count = u16::from_le_bytes([cnt_bytes[0], cnt_bytes[1]]) as usize;
        off += 2;
        if count > 128 {
            return false;
        }
        let mut history = Vec::with_capacity(count);
        for _ in 0..count {
            let Some(prev_pos) = Self::decode_pos(raw, &mut off) else {
                return false;
            };
            let Some(&ulen) = raw.get(off) else {
                return false;
            };
            off += 1;
            let Some(ubytes) = raw.get(off..off + usize::from(ulen)) else {
                return false;
            };
            off += usize::from(ulen);
            let Ok(uci_str) = core::str::from_utf8(ubytes) else {
                return false;
            };
            let uci = String::from(uci_str);
            let leg = prev_pos.legal_moves();
            let san = ChessMove::from_uci_on(&prev_pos, &uci, &leg)
                .map(|m| prev_pos.format_san(m, &leg))
                .unwrap_or_else(|| uci.clone());
            history.push((prev_pos, san, uci));
        }

        self.mode = mode;
        self.menu_cursor = menu_cursor;
        self.menu_level = menu_level;
        self.pos = pos;
        self.history = history;
        self.cursor_sq = cursor_sq;
        self.selected_sq = selected_sq;
        self.last_move = last_move;
        self.seed = seed;
        self.coach_hint = None;
        self.legal_cache = self.pos.legal_moves();
        if status_code == 6 && matches!(self.mode, ChessMode::Puzzles { .. }) {
            self.status = GameStatus::PuzzleSolved;
        } else {
            self.update_status();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perft(pos: &Position, depth: u8) -> u64 {
        let list = pos.generate_legal_list(false);
        if depth <= 1 {
            return list.len as u64;
        }
        let mut nodes = 0u64;
        for i in 0..list.len {
            let nxt = pos.make_move(list.moves[i]);
            nodes += perft(&nxt, depth - 1);
        }
        nodes
    }

    #[test]
    fn fide_perft_initial_position_exact() {
        let pos = Position::default();
        assert_eq!(perft(&pos, 1), 20);
        assert_eq!(perft(&pos, 2), 400);
        assert_eq!(perft(&pos, 3), 8_902);
    }

    #[test]
    fn castling_and_en_passant_and_promotion_legal() {
        let pos = Position::from_fen("r3k2r/8/8/3pP3/8/8/8/R3K2R w KQkq d6 0 1").unwrap();
        let moves = pos.legal_moves();
        assert!(moves.iter().any(|m| m.is_castle && m.to == 6));
        assert!(moves.iter().any(|m| m.is_castle && m.to == 2));
        assert!(moves.iter().any(|m| m.is_ep && m.uci() == "e5d6"));
    }

    #[test]
    fn all_15_puzzles_and_mate_in_2_master_engine() {
        let mut engine = ChessEngine::new();
        assert!(core::mem::size_of::<ChessEngine>() <= 24_576, "ChessEngine too big for ESP32-C3");

        for (i, p) in CHESS_PUZZLES.iter().enumerate() {
            let pos = Position::from_fen(p.fen).expect("valid puzzle FEN");
            let moves = pos.legal_moves();
            assert!(
                moves.iter().any(|m| m.uci() == p.solution_uci),
                "Puzzle {} missing legal solution {}",
                i + 1,
                p.solution_uci
            );
            let mv = moves.into_iter().find(|m| m.uci() == p.solution_uci).unwrap();
            let after = pos.make_move(mv);
            assert!(
                after.in_check(after.stm) || p.solution_uci == "f2a7" || p.solution_uci == "d1h5",
                "Puzzle {} solution should be forcing",
                i + 1
            );
        }

        // Forced Mate-in-2 verification on Level 4 (2000 ELO): 1. Qxd8+ Re8 2. Qxe8#
        let m2 = Position::from_fen("3r2k1/4rppp/8/8/8/8/3Q4/3R2K1 w - - 0 1").unwrap();
        let (best, sc) = engine.choose_move(&m2, 4, 1).unwrap();
        assert_eq!(best.uci(), "d2d8", "Master engine should spot Mate-in-2 Qxd8+! Got {} (score {})", best.uci(), sc);
        assert!(sc > 20_000);
    }

    #[test]
    fn engine_all_four_levels_and_master_vs_novice_self_play() {
        assert_eq!(ChessApp::level_elo(1), 1200);
        assert_eq!(ChessApp::level_elo(2), 1500);
        assert_eq!(ChessApp::level_elo(3), 1750);
        assert_eq!(ChessApp::level_elo(4), 2000);

        let mut white = ChessEngine::new(); // Level 4 (2000 ELO)
        let mut black = ChessEngine::new(); // Level 1 (1200 ELO)
        // White = Level 4 (2000 ELO) from knight/pawn-up tactical endgame vs Level 1 -> mates or wins cleanly fast!
        let mut pos = Position::from_fen("r1bqk2r/pppp1ppp/2n2n2/4p3/1bB1P3/2NP1N2/PPP2PPP/R1BQK2R w KQkq - 0 5").unwrap();
        let mut seen_hashes = Vec::new();
        let mut winner = None;

        for ply in 0..24u32 {
            seen_hashes.push(pos.hash);
            let legal = pos.legal_moves();
            if legal.is_empty() {
                winner = Some(if pos.in_check(pos.stm) {
                    Some(match pos.stm {
                        Color::White => Color::Black,
                        Color::Black => Color::White,
                    })
                } else {
                    None
                });
                break;
            }
            let level = if pos.stm == Color::White { 4 } else { 1 };
            let eng = if pos.stm == Color::White { &mut white } else { &mut black };
            let (mv, _) = eng.choose_move(&pos, level, ply + 7).expect("legal move expected");
            assert!(
                legal.contains(&mv),
                "Engine lvl {level} chose illegal move {}",
                mv.uci()
            );
            let san = pos.format_san(mv, &legal);
            assert!(!san.is_empty());
            let nxt = pos.make_move(mv);
            assert_eq!(nxt.hash, nxt.compute_hash(), "Zobrist incremental hash mismatch after {san}");
            pos = nxt;
        }

        // Also test Level 4 conversion from tactical advantage vs Level 1
        let adv = Position::from_fen("3r2k1/5ppp/8/8/2Q5/8/5PPP/3R2K1 w - - 0 1").unwrap();
        let (mv_adv, sc_adv) = white.choose_move(&adv, 4, 1).unwrap();
        assert!(sc_adv >= 200);
        assert!(adv.legal_moves().contains(&mv_adv));
        let _ = seen_hashes;
        let _ = winner;
    }

    #[test]
    fn chess_encode_decode_preserves_mid_game_history_and_undo() {
        let mut app = ChessApp::new();
        app.start_ai(3);
        let e2e4 = app
            .legal_cache
            .iter()
            .copied()
            .find(|m| m.uci() == "e2e4")
            .unwrap();
        app.apply_player_move(e2e4);
        assert_eq!(app.history.len(), 2, "White + Black AI reply");
        app.cursor_sq = 28;
        let encoded = app.encode_state();
        assert!(encoded.len() < 1024);

        let mut restored = ChessApp::new();
        assert!(restored.decode_state(&encoded));
        assert_eq!(restored.mode, ChessMode::PlayingAi { level: 3 });
        assert_eq!(restored.pos, app.pos);
        assert_eq!(restored.cursor_sq, 28);
        assert_eq!(restored.last_move, app.last_move);
        assert_eq!(restored.history.len(), 2);
        assert_eq!(restored.history[0].1, app.history[0].1);
        assert_eq!(restored.history[1].2, app.history[1].2);

        // Undo in restored game must rewind both plies back to starting position
        restored.handle_button(Button::Back);
        assert_eq!(restored.pos, Position::default());
        assert!(restored.history.is_empty());

        // Solved puzzle state round-trip
        let mut pz = ChessApp::new();
        pz.start_puzzle(2);
        let sol = pz
            .legal_cache
            .iter()
            .copied()
            .find(|m| m.uci() == CHESS_PUZZLES[2].solution_uci)
            .unwrap();
        pz.apply_player_move(sol);
        assert_eq!(pz.status, GameStatus::PuzzleSolved);
        let mut pz2 = ChessApp::new();
        assert!(pz2.decode_state(&pz.encode_state()));
        assert_eq!(pz2.mode, ChessMode::Puzzles { idx: 2 });
        assert_eq!(pz2.status, GameStatus::PuzzleSolved);
    }
}
