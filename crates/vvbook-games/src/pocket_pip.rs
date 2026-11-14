extern crate alloc;

use alloc::collections::{BTreeSet, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use vvbook_gfx::baked_fonts;
use vvbook_gfx::canvas::{Align, Canvas};
use vvbook_gfx::icons::{PipHat, PipMood, draw_pip_fox, draw_starberry, fill_triangle};
use vvbook_pal::{Button, PORTRAIT_W, RefreshMode, SAFE_INSETS, Tone};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameAction {
    None,
    Redraw(RefreshMode),
    ExitToHome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipTab {
    Den = 0,
    Meadow = 1,
    Memory = 2,
    Stickers = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipProgress {
    pub starberries: u16,
    pub hats_mask: u8,
    pub equipped_hat: u8,
    pub stickers_mask: u32,
    pub meadow_done_mask: u32,
    pub memory_best_moves: u16,
    pub kindness_hearts: u16,
}

impl Default for PipProgress {
    fn default() -> Self {
        Self {
            starberries: 3,
            hats_mask: 0b0000_0011, // None + FlowerCrown unlocked from start
            equipped_hat: 1,
            stickers_mask: 0b0000_0111, // First 3 welcome stickers unlocked
            meadow_done_mask: 0,
            memory_best_moves: 0,
            kindness_hearts: 2,
        }
    }
}

impl PipProgress {
    pub fn encode(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..2].copy_from_slice(&self.starberries.to_le_bytes());
        b[2] = self.hats_mask;
        b[3] = self.equipped_hat;
        b[4..8].copy_from_slice(&self.stickers_mask.to_le_bytes());
        b[8..12].copy_from_slice(&self.meadow_done_mask.to_le_bytes());
        b[12..14].copy_from_slice(&self.memory_best_moves.to_le_bytes());
        b[14..16].copy_from_slice(&self.kindness_hearts.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Self {
        if b.len() < 16 {
            return Self::default();
        }
        Self {
            starberries: u16::from_le_bytes([b[0], b[1]]),
            hats_mask: b[2],
            equipped_hat: b[3],
            stickers_mask: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            meadow_done_mask: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            memory_best_moves: u16::from_le_bytes([b[12], b[13]]),
            kindness_hearts: u16::from_le_bytes([b[14], b[15]]),
        }
    }

    pub fn unlock_next_reward(&mut self) {
        for bit in 0..24 {
            if (self.stickers_mask & (1 << bit)) == 0 {
                self.stickers_mask |= 1 << bit;
                break;
            }
        }
        for hat_idx in 1..6u8 {
            if (self.hats_mask & (1 << hat_idx)) == 0 {
                self.hats_mask |= 1 << hat_idx;
                break;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tile {
    Grass,
    Hedge,
    Stream,
    Bridge,
    Lilypad,
    Goal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MeadowState {
    pub grid: [u8; 81], // encodes base Tile: 0 Grass, 1 Hedge, 2 Stream, 3 Bridge, 4 Lilypad, 5 Goal
    pub acorns: Vec<u8>,
    pub berries: Vec<u8>,
    pub pip: u8,
    pub goal: u8,
}

impl MeadowState {
    pub fn from_level(idx: usize) -> Self {
        let lvl = &MEADOW_LEVELS[idx % MEADOW_LEVELS.len()];
        let mut grid = [0u8; 81];
        let mut acorns = Vec::new();
        let mut berries = Vec::new();
        let mut pip = 0u8;
        let mut goal = 80u8;
        for (r, row) in lvl.rows.iter().enumerate() {
            for (c, ch) in row.as_bytes().iter().take(9).enumerate() {
                let pos = (r * 9 + c) as u8;
                grid[pos as usize] = match *ch {
                    b'#' => 1,
                    b'~' => 2,
                    b'=' => 3,
                    b'L' => 4,
                    b'E' => {
                        goal = pos;
                        5
                    }
                    b'O' => {
                        acorns.push(pos);
                        0
                    }
                    b'*' => {
                        berries.push(pos);
                        0
                    }
                    b'P' => {
                        pip = pos;
                        0
                    }
                    _ => 0,
                };
            }
        }
        acorns.sort_unstable();
        berries.sort_unstable();
        Self {
            grid,
            acorns,
            berries,
            pip,
            goal,
        }
    }

    pub fn step(&self, dir: Dir) -> Option<Self> {
        let r = (self.pip / 9) as i32;
        let c = (self.pip % 9) as i32;
        let (dr, dc) = match dir {
            Dir::Up => (-1, 0),
            Dir::Down => (1, 0),
            Dir::Left => (0, -1),
            Dir::Right => (0, 1),
        };
        let nr = r + dr;
        let nc = c + dc;
        if !(0..9).contains(&nr) || !(0..9).contains(&nc) {
            return None;
        }
        let npos = (nr * 9 + nc) as u8;
        let t = self.grid[npos as usize];
        if t == 1 || t == 2 {
            // Hedge or water stream blocks walking directly
            return None;
        }

        let mut next = self.clone();
        // Stepping off a delicate lilypad turns that lilypad back into stream water
        if next.grid[self.pip as usize] == 4 {
            next.grid[self.pip as usize] = 2;
        }

        // Check if pushing an acorn
        if let Some(a_idx) = next.acorns.iter().position(|&p| p == npos) {
            let ar = nr + dr;
            let ac = nc + dc;
            if !(0..9).contains(&ar) || !(0..9).contains(&ac) {
                return None;
            }
            let apos = (ar * 9 + ac) as u8;
            let at = next.grid[apos as usize];
            if at == 1 || at == 5 || next.acorns.contains(&apos) || next.berries.contains(&apos) {
                return None;
            }
            if at == 2 {
                // Acorn splashes into the stream -> cozy solid bridge!
                next.acorns.remove(a_idx);
                next.grid[apos as usize] = 3;
            } else {
                next.acorns[a_idx] = apos;
                next.acorns.sort_unstable();
            }
        }

        // Collect starberry if present
        if let Some(b_idx) = next.berries.iter().position(|&p| p == npos) {
            next.berries.remove(b_idx);
        }
        next.pip = npos;
        Some(next)
    }

    pub fn is_won(&self) -> bool {
        self.berries.is_empty() && self.pip == self.goal
    }
}

/// Optimal BFS solver used both by Pip's Sparkle Hint and by the unit test validating all 25 levels!
pub fn solve_meadow_bfs(start: &MeadowState, max_nodes: usize) -> Option<Vec<Dir>> {
    if start.is_won() {
        return Some(Vec::new());
    }
    let mut visited: BTreeSet<MeadowState> = BTreeSet::new();
    let mut queue: VecDeque<(MeadowState, Vec<Dir>)> = VecDeque::new();
    visited.insert(start.clone());
    queue.push_back((start.clone(), Vec::new()));

    while let Some((cur, path)) = queue.pop_front() {
        if visited.len() > max_nodes {
            return None;
        }
        for dir in [Dir::Up, Dir::Down, Dir::Left, Dir::Right] {
            if let Some(nxt) = cur.step(dir) {
                let mut npath = path.clone();
                npath.push(dir);
                if nxt.is_won() {
                    return Some(npath);
                }
                if visited.insert(nxt.clone()) {
                    queue.push_back((nxt, npath));
                }
            }
        }
    }
    None
}

pub struct MeadowLevelDef {
    pub name_en: &'static str,
    pub name_ru: &'static str,
    pub rows: [&'static str; 9],
}

/// 25 handcrafted `9x9` cozy Starberry Meadow levels (gradually introducing bridges, lilypads & twin-acorn spans).
pub const MEADOW_LEVELS: [MeadowLevelDef; 25] = [
    MeadowLevelDef {
        name_en: "1. Morning Dew",
        name_ru: "1. Утренняя роса",
        rows: [
            "#########",
            "#P.*....#",
            "#..###..#",
            "#.*...*.#",
            "#..###..#",
            "#......E#",
            "#.......#",
            "#.......#",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "2. The Little Acorn",
        name_ru: "2. Маленький жёлудь",
        rows: [
            "#########",
            "#P..O~*.#",
            "#.*..~..#",
            "#...~~..#",
            "#...~~*.#",
            "#......E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "3. Three Starberries",
        name_ru: "3. Три звездоягоды",
        rows: [
            "#########",
            "#P.*.O~.#",
            "#.#..~~*#",
            "#*...~~.#",
            "#..###~.#",
            "#.....~E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "4. Stepping Lilypads",
        name_ru: "4. Кувшинки у ручья",
        rows: [
            "#########",
            "#P.~...*#",
            "#..L~~..#",
            "#*.~~L..#",
            "#..~..*.#",
            "#..L...E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "5. Twin Brooks",
        name_ru: "5. Два ручейка",
        rows: [
            "#########",
            "#P.O~*..#",
            "#..#~...#",
            "#.O~~.*.#",
            "#..#~...#",
            "#..*~..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "6. Clover Corner",
        name_ru: "6. Клеверный угол",
        rows: [
            "#########",
            "#P.*....#",
            "#.O#.#..#",
            "#..~~*.##",
            "#..~~...#",
            "#.*.#..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "7. Mossy Island",
        name_ru: "7. Мшистый островок",
        rows: [
            "#########",
            "#P..~*.E#",
            "#O..L~~.#",
            "#*..~L..#",
            "#..~~~*.#",
            "#.......#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "8. Acorn Ferry",
        name_ru: "8. Желудёвый паром",
        rows: [
            "#########",
            "#P.*#..*#",
            "#.O.#.O.#",
            "#..~~~..#",
            "#..~~~..#",
            "#...*..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "9. Whispering Willow",
        name_ru: "9. Шепчущая ива",
        rows: [
            "#########",
            "#P.#.*..#",
            "#..L.#..#",
            "#*.~...*#",
            "#..#L#..#",
            "#....#.E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "10. Sunbeam Path",
        name_ru: "10. Солнечная тропа",
        rows: [
            "#########",
            "#P.*.#.*#",
            "#.O..~..#",
            "#..#.~~.#",
            "#*O..~..#",
            "#....~E.#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "11. Dewdrop Pond",
        name_ru: "11. Пруд из росинок",
        rows: [
            "#########",
            "#P.*~...#",
            "#.O.~.*.#",
            "#...L~..#",
            "#..O~~*.#",
            "#...~..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "12. Hazel Hedge",
        name_ru: "12. Ореховая изгородь",
        rows: [
            "#########",
            "#P.*....#",
            "#.O##~#.#",
            "#..*~...#",
            "#.O.#~*.#",
            "#....~.E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "13. Paper Boat Crossing",
        name_ru: "13. Бумажный кораблик",
        rows: [
            "#########",
            "#P.*~.*.#",
            "#.O.~.O.#",
            "#...~~..#",
            "#.*.L..*#",
            "#...~..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "14. Pebble Stepping",
        name_ru: "14. Камешки в ручье",
        rows: [
            "#########",
            "#P.L~..*#",
            "#*..~L..#",
            "#.O.~~..#",
            "#..O~.*.#",
            "#..L~..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "15. Foxtail Bend",
        name_ru: "15. Лисья излучина",
        rows: [
            "#########",
            "#P*...#.#",
            "#.###...#",
            "#.O.~~~*#",
            "#..O~~..#",
            "#*..L..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "16. Firefly Hollow",
        name_ru: "16. Ложбинка светлячков",
        rows: [
            "#########",
            "#P.O~..*#",
            "#*..L~..#",
            "#..#~L#.#",
            "#.O.~.*.#",
            "#...L..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "17. Fern Labyrinth",
        name_ru: "17. Папоротниковый сад",
        rows: [
            "#########",
            "#P.*.#.*#",
            "#.##.#.##",
            "#..O.~..#",
            "#.O#~L*.#",
            "#....L.E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "18. Silver Mist",
        name_ru: "18. Серебряный туман",
        rows: [
            "#########",
            "#P.O~.*.#",
            "#*..~~#.#",
            "#...~~..#",
            "#.O.L~*.#",
            "#...~L.E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "19. Berry Basket",
        name_ru: "19. Корзинка ягод",
        rows: [
            "#########",
            "#P*O~.*.#",
            "#...~~..#",
            "#.#.~~#.#",
            "#..O~~*.#",
            "#...L..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "20. Brookside Bridge",
        name_ru: "20. Мостик у бобра",
        rows: [
            "#########",
            "#P..#.*.#",
            "#.O.L~~.#",
            "#*..~L..#",
            "#.O.~L~*#",
            "#....~.E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "21. Chestnut Grove",
        name_ru: "21. Каштановая роща",
        rows: [
            "#########",
            "#P*O~..*#",
            "#..#~~#.#",
            "#.O.~~..#",
            "#..*L~L.#",
            "#...~..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "22. Moonlit Stream",
        name_ru: "22. Лунный ручей",
        rows: [
            "#########",
            "#P.O~*.E#",
            "#...L~#.#",
            "#*..~~..#",
            "#.O.L~*.#",
            "#...~...#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "23. Sleepy Hollow",
        name_ru: "23. Сонное дупло",
        rows: [
            "#########",
            "#P.*~.*.#",
            "#.O.~~#.#",
            "#..#L~..#",
            "#.O.~.*.#",
            "#...L..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "24. Golden Autumn",
        name_ru: "24. Золотая осень",
        rows: [
            "#########",
            "#P*O~..*#",
            "#...L~#.#",
            "#.O.~~..#",
            "#...L*L.#",
            "#...~..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
    MeadowLevelDef {
        name_en: "25. Starberry Festival",
        name_ru: "25. Праздник Звездоягод",
        rows: [
            "#########",
            "#P*O~*..#",
            "#..#~~#.#",
            "#.O.L~*.#",
            "#...L~..#",
            "#..#~..E#",
            "#########",
            "#########",
            "#########",
        ],
    },
];

pub const STICKER_NAMES: [(&str, &str); 24] = [
    ("Starberry", "Звездоягода"),
    ("Golden Acorn", "Золотой жёлудь"),
    ("Fox Ribbon", "Лисий бантик"),
    ("Paper Boat", "Бумажный кораблик"),
    ("Lilypad", "Кувшинка"),
    ("Owl Feather", "Перо совы"),
    ("Warm Cocoa", "Тёплое какао"),
    ("Moon Lantern", "Лунный фонарик"),
    ("Tiny Crown", "Корона"),
    ("Bluebell", "Колокольчик"),
    ("Oak Leaf", "Дубовый лист"),
    ("Starry Scarf", "Звёздный шарф"),
    ("Honey Jar", "Баночка мёда"),
    ("Music Note", "Нотка"),
    ("Pocket Book", "Книжка-малышка"),
    ("Rainbow Snail", "Улитка"),
    ("Snowflake", "Снежинка"),
    ("Apple Pie", "Яблочный пирог"),
    ("Comet", "Комета"),
    ("Butterfly", "Бабочка"),
    ("Teapot", "Заварник"),
    ("Telescope", "Телескоп"),
    ("Clover", "Четырёхлистник"),
    ("Oak Crown", "Корона дуба"),
];

pub struct MemoryGame {
    pub cards: [u8; 16],     // symbol 0..8
    pub revealed: [bool; 16],
    pub matched: [bool; 16],
    pub cursor: usize,
    pub first_pick: Option<usize>,
    pub second_pick: Option<usize>,
    pub moves: u16,
    pub two_player: bool,
    pub turn_p2: bool,
    pub score_p1: u8,
    pub score_p2: u8,
}

impl MemoryGame {
    pub fn new(seed: u32, two_player: bool) -> Self {
        let mut cards = [0u8; 16];
        for i in 0..8 {
            cards[i * 2] = i as u8;
            cards[i * 2 + 1] = i as u8;
        }
        // Deterministic Fisher-Yates shuffle
        let mut s = seed.wrapping_add(0x9E37_79B9);
        for i in (1..16).rev() {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let j = (s as usize) % (i + 1);
            cards.swap(i, j);
        }
        Self {
            cards,
            revealed: [false; 16],
            matched: [false; 16],
            cursor: 0,
            first_pick: None,
            second_pick: None,
            moves: 0,
            two_player,
            turn_p2: false,
            score_p1: 0,
            score_p2: 0,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.matched.iter().all(|&m| m)
    }

    pub fn on_confirm(&mut self) -> Option<bool> {
        if let (Some(a), Some(b)) = (self.first_pick, self.second_pick) {
            self.revealed[a] = false;
            self.revealed[b] = false;
            self.first_pick = None;
            self.second_pick = None;
            if self.two_player {
                self.turn_p2 = !self.turn_p2;
            }
            return None;
        }
        let c = self.cursor;
        if self.matched[c] || self.revealed[c] {
            return None;
        }
        self.revealed[c] = true;
        if let Some(a) = self.first_pick {
            self.moves += 1;
            if self.cards[a] == self.cards[c] {
                self.matched[a] = true;
                self.matched[c] = true;
                self.first_pick = None;
                if self.turn_p2 {
                    self.score_p2 += 1;
                } else {
                    self.score_p1 += 1;
                }
                if self.is_finished() {
                    return Some(true);
                }
            } else {
                self.second_pick = Some(c);
            }
        } else {
            self.first_pick = Some(c);
        }
        None
    }
}

pub struct PocketPipApp {
    pub progress: PipProgress,
    pub tab: PipTab,
    pub den_cursor: usize,
    pub mood: PipMood,
    pub level_idx: usize,
    pub meadow: MeadowState,
    pub undo_stack: Vec<MeadowState>,
    pub hint_dir: Option<Dir>,
    pub memory: MemoryGame,
    pub sticker_cursor: usize,
    pub status_banner: String,
    pub seed_counter: u32,
}

impl PocketPipApp {
    pub fn new(progress: PipProgress) -> Self {
        Self {
            progress,
            tab: PipTab::Den,
            den_cursor: 0,
            mood: PipMood::Happy,
            level_idx: 0,
            meadow: MeadowState::from_level(0),
            undo_stack: Vec::new(),
            hint_dir: None,
            memory: MemoryGame::new(42, false),
            sticker_cursor: 0,
            status_banner: String::new(),
            seed_counter: 1,
        }
    }

    pub fn award_reading_starberry(&mut self) {
        self.progress.starberries = self.progress.starberries.saturating_add(1);
        self.mood = PipMood::Celebrating;
    }

    fn equipped_hat(&self) -> PipHat {
        PipHat::ALL[self.progress.equipped_hat as usize % PipHat::ALL.len()]
    }

    pub fn load_level(&mut self, idx: usize) {
        self.level_idx = idx % MEADOW_LEVELS.len();
        self.meadow = MeadowState::from_level(self.level_idx);
        self.undo_stack.clear();
        self.hint_dir = None;
        self.status_banner.clear();
    }

    pub fn handle_button(&mut self, btn: Button, is_ru: bool) -> GameAction {
        self.hint_dir = None;
        match self.tab {
            PipTab::Den => match btn {
                Button::Back => return GameAction::ExitToHome,
                Button::Up | Button::Left => {
                    self.den_cursor = (self.den_cursor + 4) % 5;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Down | Button::Right => {
                    self.den_cursor = (self.den_cursor + 1) % 5;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Confirm => {
                    match self.den_cursor {
                        0 => {
                            // Pet / Feed Pip a Starberry!
                            if self.progress.starberries > 0 {
                                self.progress.starberries -= 1;
                                self.progress.kindness_hearts += 1;
                                self.mood = PipMood::Celebrating;
                                self.status_banner = if is_ru {
                                    "Пип счастлив! +1 сердечко!".into()
                                } else {
                                    "Pip loved the Starberry! +1 Heart!".into()
                                };
                            } else {
                                self.mood = PipMood::Happy;
                                self.status_banner = if is_ru {
                                    "Почитай 5 страниц, чтобы найти звездоягоду!".into()
                                } else {
                                    "Read 5 book pages to find a Starberry!".into()
                                };
                            }
                        }
                        1 => {
                            // Cycle unlocked hats
                            let cur = self.progress.equipped_hat;
                            for step in 1..=6u8 {
                                let cand = (cur + step) % 6;
                                if (self.progress.hats_mask & (1 << cand)) != 0 {
                                    self.progress.equipped_hat = cand;
                                    break;
                                }
                            }
                            self.mood = PipMood::Curious;
                        }
                        2 => {
                            // Play Starberry Meadow
                            let next_unsolved = (0..MEADOW_LEVELS.len())
                                .find(|&i| (self.progress.meadow_done_mask & (1 << i)) == 0)
                                .unwrap_or(0);
                            self.load_level(next_unsolved);
                            self.tab = PipTab::Meadow;
                            return GameAction::Redraw(RefreshMode::FastDu);
                        }
                        3 => {
                            self.seed_counter = self.seed_counter.wrapping_add(17);
                            self.memory = MemoryGame::new(self.seed_counter, false);
                            self.tab = PipTab::Memory;
                            return GameAction::Redraw(RefreshMode::FastDu);
                        }
                        _ => {
                            self.tab = PipTab::Stickers;
                            return GameAction::Redraw(RefreshMode::FastDu);
                        }
                    }
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                _ => {}
            },
            PipTab::Meadow => {
                if self.meadow.is_won() {
                    if btn == Button::Confirm {
                        self.load_level((self.level_idx + 1) % MEADOW_LEVELS.len());
                        return GameAction::Redraw(RefreshMode::FastDu);
                    } else if btn == Button::Back {
                        self.tab = PipTab::Den;
                        return GameAction::Redraw(RefreshMode::FastDu);
                    }
                    return GameAction::None;
                }
                let move_dir = match btn {
                    Button::Up => Some(Dir::Up),
                    Button::Down => Some(Dir::Down),
                    Button::Left => Some(Dir::Left),
                    Button::Right => Some(Dir::Right),
                    Button::Back => {
                        if let Some(prev) = self.undo_stack.pop() {
                            self.meadow = prev;
                            return GameAction::Redraw(RefreshMode::FastDu);
                        } else {
                            self.tab = PipTab::Den;
                            return GameAction::Redraw(RefreshMode::FastDu);
                        }
                    }
                    Button::Confirm => {
                        // Compute Sparkle Hint via BFS!
                        if let Some(path) = solve_meadow_bfs(&self.meadow, 2500) {
                            self.hint_dir = path.first().copied();
                            self.status_banner = if is_ru {
                                "Искорка-подсказка показала путь!".into()
                            } else {
                                "Sparkle Hint shows the next step!".into()
                            };
                        } else {
                            // Dead end? Gently undo back or restart!
                            self.load_level(self.level_idx);
                            self.status_banner = if is_ru {
                                "Уровень сброшен с начала.".into()
                            } else {
                                "Level reset fresh!".into()
                            };
                        }
                        return GameAction::Redraw(RefreshMode::FastDu);
                    }
                    _ => None,
                };
                if let Some(d) = move_dir
                    && let Some(next) = self.meadow.step(d) {
                        self.undo_stack.push(self.meadow.clone());
                        self.meadow = next;
                        if self.meadow.is_won() {
                            let bit = 1u32 << (self.level_idx as u32);
                            if (self.progress.meadow_done_mask & bit) == 0 {
                                self.progress.meadow_done_mask |= bit;
                                self.progress.starberries += 2;
                                self.progress.unlock_next_reward();
                            }
                            self.mood = PipMood::Celebrating;
                            return GameAction::Redraw(RefreshMode::FastDu);
                        }
                        return GameAction::Redraw(RefreshMode::FastDu);
                    }
            }
            PipTab::Memory => match btn {
                Button::Back => {
                    self.tab = PipTab::Den;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Left => {
                    self.memory.cursor = (self.memory.cursor + 15) % 16;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Right => {
                    self.memory.cursor = (self.memory.cursor + 1) % 16;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Up => {
                    self.memory.cursor = (self.memory.cursor + 12) % 16;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Down => {
                    self.memory.cursor = (self.memory.cursor + 4) % 16;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Confirm => {
                    if self.memory.is_finished() {
                        // Toggle 1P / 2P and start fresh round
                        self.seed_counter = self.seed_counter.wrapping_add(31);
                        self.memory = MemoryGame::new(self.seed_counter, !self.memory.two_player);
                        return GameAction::Redraw(RefreshMode::FastDu);
                    }
                    if let Some(true) = self.memory.on_confirm() {
                        self.progress.starberries += 1;
                        if self.progress.memory_best_moves == 0 || self.memory.moves < self.progress.memory_best_moves {
                            self.progress.memory_best_moves = self.memory.moves;
                        }
                    }
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                _ => {}
            },
            PipTab::Stickers => match btn {
                Button::Back | Button::Confirm => {
                    self.tab = PipTab::Den;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Left => {
                    self.sticker_cursor = (self.sticker_cursor + 23) % 24;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Right => {
                    self.sticker_cursor = (self.sticker_cursor + 1) % 24;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Up => {
                    self.sticker_cursor = (self.sticker_cursor + 20) % 24;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                Button::Down => {
                    self.sticker_cursor = (self.sticker_cursor + 4) % 24;
                    return GameAction::Redraw(RefreshMode::FastDu);
                }
                _ => {}
            },
        }
        GameAction::None
    }

    pub fn render(&self, canvas: &mut Canvas<'_>, is_ru: bool) {
        let top = SAFE_INSETS.top as i32 + 52;
        match self.tab {
            PipTab::Den => self.render_den(canvas, top, is_ru),
            PipTab::Meadow => self.render_meadow(canvas, top, is_ru),
            PipTab::Memory => self.render_memory(canvas, top, is_ru),
            PipTab::Stickers => self.render_stickers(canvas, top, is_ru),
        }
    }

    fn render_den(&self, canvas: &mut Canvas<'_>, top: i32, is_ru: bool) {
        let left = 36;
        let col_w = PORTRAIT_W as i32 - 72;
        let title = if is_ru { "Лисёнок Пип" } else { "Pocket Pip" };
        canvas.draw_text(&baked_fonts::FONT_UI_32_BOLD, left, top + 35, title, Tone::Ink);

        let sb_text = format!("★ {} · ♥ {}", self.progress.starberries, self.progress.kindness_hearts);
        canvas.draw_text_aligned(
            &baked_fonts::FONT_MONO_20_SEMI,
            left + col_w,
            top + 34,
            &sb_text,
            Tone::Ink,
            Align::Right,
        );
        canvas.fill_rect(left, top + 49, col_w, 3, Tone::Ink);

        // Pip character on open paper over a clean datum rule
        draw_pip_fox(
            canvas,
            PORTRAIT_W as i32 / 2,
            top + 148,
            92,
            self.mood,
            self.equipped_hat(),
        );
        canvas.hline(left, top + 234, col_w, Tone::Ink);

        let mood_line = if !self.status_banner.is_empty() {
            self.status_banner.as_str()
        } else if is_ru {
            "Тёплый чай и тихие страницы"
        } else {
            "Warm tea and unhurried pages"
        };
        canvas.draw_text_aligned(
            &baked_fonts::FONT_SERIF_23_ITALIC,
            PORTRAIT_W as i32 / 2,
            top + 266,
            mood_line,
            Tone::Ink,
            Align::Center,
        );

        let items = if is_ru {
            [
                ("Угостить Пипа (-1 ★)", "Подарить заботу и сердечко"),
                ("Примерить шапочку", "Коллекция шляп и венков"),
                ("Звездоягодный луг (25)", "Головоломки с желудями"),
                ("Цветочная память", "Сыграй одна или с папой"),
                ("Альбом наклеек (24)", "Воспоминания о книгах"),
            ]
        } else {
            [
                ("Starberry Treat (-1 ★)", "Share kindness & earn a heart"),
                ("Swap Cozy Hat", "Try on unlocked hats & crowns"),
                ("Starberry Meadow (25)", "Acorns, streams & lilypads"),
                ("Memory Blossom", "Solo or together with Dad"),
                ("Sticker Album (24)", "Keepsakes from reading & play"),
            ]
        };

        let row_h = 62;
        let mut y = top + 294;
        for (idx, (head, sub)) in items.iter().enumerate() {
            let selected = idx == self.den_cursor;
            if selected {
                canvas.fill_rect(left, y, col_w, row_h, Tone::Ink);
            }
            let fg = if selected { Tone::Paper } else { Tone::Ink };
            let num = format!("{:02}", idx + 1);
            canvas.draw_text(&baked_fonts::FONT_MONO_20_SEMI, left + 14, y + 26, &num, fg);
            canvas.draw_text_truncated(&baked_fonts::FONT_UI_22_SEMI, left + 52, y + 27, col_w - 66, head, fg);
            canvas.draw_text_truncated(&baked_fonts::FONT_UI_19_SEMI, left + 52, y + 51, col_w - 66, sub, fg);
            if !selected && idx + 1 < items.len() && idx + 1 != self.den_cursor {
                canvas.hline_dotted(left, y + row_h + 3, col_w, 2, Tone::Ink);
            }
            y += row_h + 6;
        }

        let chin = if is_ru {
            ["Домой", "Открыть", "Выше", "Ниже"]
        } else {
            ["Home", "Select", "Prev", "Next"]
        };
        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, chin, Some(1));
    }

    fn render_meadow(&self, canvas: &mut Canvas<'_>, top: i32, is_ru: bool) {
        let left = 36;
        let col_w = PORTRAIT_W as i32 - 72;
        let lvl = &MEADOW_LEVELS[self.level_idx];
        let name = if is_ru { lvl.name_ru } else { lvl.name_en };
        canvas.draw_text_truncated(&baked_fonts::FONT_UI_25_BOLD, left, top + 28, 320, name, Tone::Ink);

        let berries_left = self.meadow.berries.len();
        let left_str = if is_ru {
            format!("★ {berries_left}")
        } else {
            format!("LEFT {berries_left}")
        };
        canvas.draw_text_aligned(
            &baked_fonts::FONT_MONO_20_SEMI,
            left + col_w,
            top + 28,
            &left_str,
            Tone::Ink,
            Align::Right,
        );
        canvas.fill_rect(left, top + 40, col_w, 2, Tone::Ink);

        let cell = 48;
        let board_px = cell * 9;
        let bx = (PORTRAIT_W as i32 - board_px) / 2;
        let by = top + 58;
        canvas.stroke_rect(bx - 2, by - 2, board_px + 4, board_px + 4, 2, Tone::Ink);

        for r in 0..9 {
            for c in 0..9 {
                let pos = (r * 9 + c) as u8;
                let x = bx + c * cell;
                let y = by + r * cell;
                let tile = self.meadow.grid[pos as usize];

                match tile {
                    1 => {
                        canvas.fill_rect(x + 2, y + 2, cell - 4, cell - 4, Tone::Ink);
                        canvas.fill_circle(x + 14, y + 16, 6, Tone::Paper);
                        canvas.fill_circle(x + 32, y + 30, 6, Tone::Paper);
                    }
                    2 => {
                        canvas.fill_stipple_25(x + 1, y + 1, cell - 2, cell - 2, Tone::Ink, Tone::Paper);
                        canvas.draw_line(x + 7, y + 16, x + 20, y + 12, Tone::Ink);
                        canvas.draw_line(x + 20, y + 12, x + 36, y + 16, Tone::Ink);
                        canvas.draw_line(x + 10, y + 32, x + 26, y + 28, Tone::Ink);
                        canvas.draw_line(x + 26, y + 28, x + 41, y + 32, Tone::Ink);
                    }
                    3 => {
                        canvas.fill_stipple_25(x + 1, y + 1, cell - 2, cell - 2, Tone::Ink, Tone::Paper);
                        canvas.fill_rect(x + 4, y + 7, cell - 8, cell - 14, Tone::Paper);
                        canvas.stroke_rect(x + 4, y + 7, cell - 8, cell - 14, 2, Tone::Ink);
                        canvas.vline(x + 16, y + 9, cell - 18, Tone::Ink);
                        canvas.vline(x + 31, y + 9, cell - 18, Tone::Ink);
                    }
                    4 => {
                        canvas.fill_stipple_25(x + 1, y + 1, cell - 2, cell - 2, Tone::Ink, Tone::Paper);
                        canvas.fill_circle(x + cell / 2, y + cell / 2, 16, Tone::Paper);
                        canvas.stroke_circle(x + cell / 2, y + cell / 2, 16, 2, Tone::Ink);
                        fill_triangle(
                            canvas,
                            x + cell / 2,
                            y + cell / 2,
                            x + cell / 2 + 16,
                            y + cell / 2 - 5,
                            x + cell / 2 + 16,
                            y + cell / 2 + 5,
                            Tone::Ink,
                        );
                    }
                    5 => {
                        let open = self.meadow.berries.is_empty();
                        canvas.fill_rect(x + 5, y + 5, cell - 10, cell - 10, Tone::Ink);
                        canvas.fill_circle(x + cell / 2, y + cell / 2 + 4, 9, Tone::Paper);
                        if !open {
                            let lx = x + cell / 2;
                            let ly = y + cell / 2 + 4;
                            canvas.stroke_circle(lx, ly - 3, 4, 2, Tone::Ink);
                            canvas.fill_rect(lx - 5, ly - 1, 10, 8, Tone::Ink);
                        }
                    }
                    _ => {
                        canvas.set_pixel(x + cell / 2, y + cell / 2, Tone::Ink);
                    }
                }

                if self.meadow.acorns.contains(&pos) {
                    canvas.fill_circle(x + cell / 2, y + cell / 2 + 3, 13, Tone::Paper);
                    canvas.stroke_circle(x + cell / 2, y + cell / 2 + 3, 13, 2, Tone::Ink);
                    canvas.fill_rect(x + cell / 2 - 14, y + cell / 2 - 11, 28, 9, Tone::Ink);
                    canvas.vline(x + cell / 2, y + cell / 2 - 16, 6, Tone::Ink);
                }

                if self.meadow.berries.contains(&pos) {
                    draw_starberry(canvas, x + cell / 2, y + cell / 2, 12, true);
                }

                if self.meadow.pip == pos {
                    draw_pip_fox(
                        canvas,
                        x + cell / 2,
                        y + cell / 2 - 2,
                        34,
                        if self.meadow.is_won() { PipMood::Celebrating } else { PipMood::Happy },
                        self.equipped_hat(),
                    );
                }
            }
        }

        // Sparkle Hint directional ring
        if let Some(dir) = self.hint_dir {
            let pr = (self.meadow.pip / 9) as i32;
            let pc = (self.meadow.pip % 9) as i32;
            let (dr, dc) = match dir {
                Dir::Up => (-1, 0),
                Dir::Down => (1, 0),
                Dir::Left => (0, -1),
                Dir::Right => (0, 1),
            };
            let hx = bx + (pc + dc) * cell + cell / 2;
            let hy = by + (pr + dr) * cell + cell / 2;
            canvas.stroke_circle(hx, hy, 18, 2, Tone::Ink);
            canvas.stroke_circle(hx, hy, 13, 1, Tone::Ink);
        }

        let footer_y = by + board_px + 34;
        if self.meadow.is_won() {
            let msg = if is_ru {
                "Ура! +2 ★ · нажмите OK для следующего"
            } else {
                "Cleared! +2 ★ · press Confirm for next"
            };
            canvas.fill_rect(left, footer_y, col_w, 48, Tone::Ink);
            canvas.draw_text_aligned(
                &baked_fonts::FONT_UI_22_SEMI,
                PORTRAIT_W as i32 / 2,
                footer_y + 31,
                msg,
                Tone::Paper,
                Align::Center,
            );
        } else {
            let tip = if !self.status_banner.is_empty() {
                self.status_banner.as_str()
            } else if is_ru {
                "Боковые кнопки: вверх / вниз"
            } else {
                "Side buttons: up / down"
            };
            canvas.draw_text_aligned(
                &baked_fonts::FONT_UI_19_SEMI,
                PORTRAIT_W as i32 / 2,
                footer_y + 24,
                tip,
                Tone::Ink,
                Align::Center,
            );
        }

        let chin = if self.meadow.is_won() {
            if is_ru {
                ["Домик", "Дальше", "Влево", "Вправо"]
            } else {
                ["Den", "Next", "Left", "Right"]
            }
        } else if is_ru {
            ["Отмена", "Совет", "Влево", "Вправо"]
        } else {
            ["Undo", "Hint", "Left", "Right"]
        };
        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, chin, Some(1));
    }

    fn render_memory(&self, canvas: &mut Canvas<'_>, top: i32, is_ru: bool) {
        let left = 36;
        let col_w = PORTRAIT_W as i32 - 72;
        let title = if is_ru { "Цветочная память" } else { "Memory Blossom" };
        canvas.draw_text(&baked_fonts::FONT_UI_25_BOLD, left, top + 28, title, Tone::Ink);

        let info = if self.memory.two_player {
            if is_ru {
                format!("Я:{} П:{} {}", self.memory.score_p1, self.memory.score_p2, if self.memory.turn_p2 { "(П)" } else { "(Т)" })
            } else {
                format!("1P:{} 2P:{} {}", self.memory.score_p1, self.memory.score_p2, if self.memory.turn_p2 { "(2P)" } else { "(1P)" })
            }
        } else if is_ru {
            format!("Ходы: {}", self.memory.moves)
        } else {
            format!("MOVES {}", self.memory.moves)
        };
        canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, left + col_w, top + 28, &info, Tone::Ink, Align::Right);
        canvas.fill_rect(left, top + 40, col_w, 2, Tone::Ink);

        let card_s = 96;
        let gap = 12;
        let grid_w = card_s * 4 + gap * 3;
        let gx = (PORTRAIT_W as i32 - grid_w) / 2;
        let gy = top + 68;
        let icons_lbl = ["★", "♥", "◆", "✓", "▲", "", "", ""];

        for i in 0..16 {
            let r = (i / 4) as i32;
            let c = (i % 4) as i32;
            let x = gx + c * (card_s + gap);
            let y = gy + r * (card_s + gap);
            let is_cur = i == self.memory.cursor;
            let face_up = self.memory.matched[i] || self.memory.revealed[i];

            if face_up {
                canvas.fill_rect(x, y, card_s, card_s, Tone::Paper);
                canvas.stroke_rect(x, y, card_s, card_s, if is_cur { 3 } else { 1 }, Tone::Ink);
                let sym = self.memory.cards[i] as usize % 8;
                let cx = x + card_s / 2;
                let cy = y + card_s / 2;
                match sym {
                    0..=4 => {
                        canvas.draw_text_aligned(
                            &baked_fonts::FONT_UI_44_BOLD,
                            cx,
                            cy + 16,
                            icons_lbl[sym],
                            Tone::Ink,
                            Align::Center,
                        );
                    }
                    5 => {
                        canvas.fill_circle(cx - 2, cy, 18, Tone::Ink);
                        canvas.fill_circle(cx + 8, cy - 4, 15, Tone::Paper);
                    }
                    6 => {
                        fill_triangle(canvas, cx - 18, cy + 12, cx + 18, cy + 12, cx, cy - 22, Tone::Ink);
                        canvas.fill_rect(cx - 4, cy + 12, 8, 10, Tone::Ink);
                    }
                    _ => {
                        draw_starberry(canvas, cx, cy, 18, true);
                    }
                }
            } else {
                canvas.fill_rect(x, y, card_s, card_s, Tone::Ink);
                if is_cur {
                    canvas.stroke_rect(x - 3, y - 3, card_s + 6, card_s + 6, 2, Tone::Ink);
                }
                canvas.draw_circle_dots_card(x + card_s / 2, y + card_s / 2);
            }
        }

        let fy = gy + grid_w + 40;
        let msg = if self.memory.is_finished() {
            if is_ru {
                "Ура! +1 ★ · нажмите OK для новой партии"
            } else {
                "Blossom cleared! +1 ★ · press Confirm"
            }
        } else if is_ru {
            "Боковые кнопки выбирают ряд"
        } else {
            "Side buttons move rows · Confirm flips"
        };
        canvas.draw_text_aligned(&baked_fonts::FONT_UI_19_SEMI, PORTRAIT_W as i32 / 2, fy, msg, Tone::Ink, Align::Center);

        let chin = if self.memory.is_finished() {
            if is_ru {
                ["Домик", "Новая", "Влево", "Вправо"]
            } else {
                ["Den", "Again", "Left", "Right"]
            }
        } else if is_ru {
            ["Домик", "Открыть", "Влево", "Вправо"]
        } else {
            ["Den", "Flip", "Left", "Right"]
        };
        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, chin, Some(1));
    }

    fn render_stickers(&self, canvas: &mut Canvas<'_>, top: i32, is_ru: bool) {
        let left = 36;
        let col_w = PORTRAIT_W as i32 - 72;
        let title = if is_ru { "Альбом наклеек" } else { "Sticker Album" };
        let unlocked = self.progress.stickers_mask.count_ones();
        canvas.draw_text(&baked_fonts::FONT_UI_25_BOLD, left, top + 28, title, Tone::Ink);
        canvas.draw_text_aligned(
            &baked_fonts::FONT_MONO_20_SEMI,
            left + col_w,
            top + 28,
            &format!("{unlocked:02}/24"),
            Tone::Ink,
            Align::Right,
        );
        canvas.fill_rect(left, top + 40, col_w, 2, Tone::Ink);

        let cols = 4;
        let cell_w = 104;
        let cell_h = 80;
        let gap = 12;
        let gx = (PORTRAIT_W as i32 - (cols * cell_w + (cols - 1) * gap)) / 2;
        let gy = top + 56;

        for (i, _names) in STICKER_NAMES.iter().enumerate() {
            let r = (i as i32) / cols;
            let c = (i as i32) % cols;
            let x = gx + c * (cell_w + gap);
            let y = gy + r * (cell_h + gap);
            let has = (self.progress.stickers_mask & (1 << i)) != 0;
            let is_cur = i == self.sticker_cursor;

            if has {
                canvas.fill_rect(x, y, cell_w, cell_h, Tone::Paper);
            } else {
                canvas.fill_stipple_25(x, y, cell_w, cell_h, Tone::Ink, Tone::Paper);
            }
            canvas.stroke_rect(x, y, cell_w, cell_h, if is_cur { 3 } else { 1 }, Tone::Ink);
            if has {
                draw_starberry(canvas, x + cell_w / 2, y + 32, 12, true);
            } else {
                canvas.draw_text_aligned(&baked_fonts::FONT_UI_25_BOLD, x + cell_w / 2, y + 40, "?", Tone::Ink, Align::Center);
            }
            let num = format!("{:02}", i + 1);
            canvas.draw_text_aligned(&baked_fonts::FONT_MONO_20_SEMI, x + cell_w / 2, y + cell_h - 10, &num, Tone::Ink, Align::Center);
        }

        let (lbl_en, lbl_ru) = STICKER_NAMES[self.sticker_cursor % STICKER_NAMES.len()];
        let sel_name = if is_ru { lbl_ru } else { lbl_en };
        let has_sel = (self.progress.stickers_mask & (1 << self.sticker_cursor)) != 0;
        let bar_y = gy + 6 * (cell_h + gap) + 12;
        canvas.fill_rect(left, bar_y, col_w, 2, Tone::Ink);
        let caption = if has_sel {
            format!("★ #{:02} · {}", self.sticker_cursor + 1, sel_name)
        } else {
            format!("#{:02} · {} (?)", self.sticker_cursor + 1, sel_name)
        };
        canvas.draw_text_aligned(&baked_fonts::FONT_UI_22_SEMI, PORTRAIT_W as i32 / 2, bar_y + 32, &caption, Tone::Ink, Align::Center);

        let chin = if is_ru {
            ["Домик", "", "Влево", "Вправо"]
        } else {
            ["Den", "", "Left", "Right"]
        };
        canvas.draw_chin_bar(&baked_fonts::FONT_UI_19_SEMI, chin, None);
    }

    fn encode_meadow(m: &MeadowState, out: &mut Vec<u8>) {
        out.extend_from_slice(&m.grid);
        out.push(m.pip);
        out.push(m.goal);
        let alen = m.acorns.len().min(64) as u8;
        out.push(alen);
        out.extend_from_slice(&m.acorns[..usize::from(alen)]);
        let blen = m.berries.len().min(64) as u8;
        out.push(blen);
        out.extend_from_slice(&m.berries[..usize::from(blen)]);
    }

    fn decode_meadow(raw: &[u8], off: &mut usize) -> Option<MeadowState> {
        let g = raw.get(*off..*off + 81)?;
        *off += 81;
        let mut grid = [0u8; 81];
        for (dst, &v) in grid.iter_mut().zip(g.iter()) {
            *dst = if v <= 5 { v } else { 0 };
        }
        let &pip = raw.get(*off)?;
        *off += 1;
        let &goal = raw.get(*off)?;
        *off += 1;
        if pip >= 81 || goal >= 81 {
            return None;
        }
        let &alen = raw.get(*off)?;
        *off += 1;
        let a_slice = raw.get(*off..*off + usize::from(alen))?;
        *off += usize::from(alen);
        if a_slice.iter().any(|&p| p >= 81) {
            return None;
        }
        let &blen = raw.get(*off)?;
        *off += 1;
        let b_slice = raw.get(*off..*off + usize::from(blen))?;
        *off += usize::from(blen);
        if b_slice.iter().any(|&p| p >= 81) {
            return None;
        }
        Some(MeadowState {
            grid,
            acorns: a_slice.to_vec(),
            berries: b_slice.to_vec(),
            pip,
            goal,
        })
    }

    /// Encode full Pocket Pip progress, active Meadow puzzle + undo stack, and Memory game for SD persistence.
    pub fn encode_state(&self) -> Vec<u8> {
        let keep_undo = self.undo_stack.len().min(48);
        let undo_slice = &self.undo_stack[self.undo_stack.len() - keep_undo..];
        let mut out = Vec::with_capacity(200 + undo_slice.len() * 96);
        out.extend_from_slice(b"VVPP");
        out.push(1); // version
        out.extend_from_slice(&self.progress.encode());
        out.push(self.tab as u8);
        out.push(self.den_cursor as u8);
        out.push(match self.mood {
            PipMood::Happy => 0,
            PipMood::Curious => 1,
            PipMood::Sleeping => 2,
            PipMood::Celebrating => 3,
            PipMood::Reading => 4,
        });
        out.push(self.level_idx as u8);
        out.push(self.sticker_cursor as u8);
        out.extend_from_slice(&self.seed_counter.to_le_bytes());

        Self::encode_meadow(&self.meadow, &mut out);
        out.push(undo_slice.len() as u8);
        for st in undo_slice {
            Self::encode_meadow(st, &mut out);
        }

        // Memory game state
        out.extend_from_slice(&self.memory.cards);
        let mut rev_mask = 0u16;
        let mut match_mask = 0u16;
        for i in 0..16 {
            if self.memory.revealed[i] {
                rev_mask |= 1 << i;
            }
            if self.memory.matched[i] {
                match_mask |= 1 << i;
            }
        }
        out.extend_from_slice(&rev_mask.to_le_bytes());
        out.extend_from_slice(&match_mask.to_le_bytes());
        out.push(self.memory.cursor as u8);
        out.push(self.memory.first_pick.map(|v| v as u8).unwrap_or(0xFF));
        out.push(self.memory.second_pick.map(|v| v as u8).unwrap_or(0xFF));
        out.extend_from_slice(&self.memory.moves.to_le_bytes());
        let flags = u8::from(self.memory.two_player) | (u8::from(self.memory.turn_p2) << 1);
        out.push(flags);
        out.push(self.memory.score_p1);
        out.push(self.memory.score_p2);
        out
    }

    /// Restore full Pocket Pip state from `encode_state` bytes.
    pub fn decode_state(&mut self, raw: &[u8]) -> bool {
        if raw.len() < 140 || &raw[0..4] != b"VVPP" || raw[4] != 1 {
            return false;
        }
        let progress = PipProgress::decode(&raw[5..21]);
        let tab = match raw[21] {
            1 => PipTab::Meadow,
            2 => PipTab::Memory,
            3 => PipTab::Stickers,
            _ => PipTab::Den,
        };
        let den_cursor = (raw[22] as usize) % 5;
        let mood = match raw[23] {
            1 => PipMood::Curious,
            2 => PipMood::Sleeping,
            3 => PipMood::Celebrating,
            4 => PipMood::Reading,
            _ => PipMood::Happy,
        };
        let level_idx = (raw[24] as usize) % MEADOW_LEVELS.len();
        let sticker_cursor = (raw[25] as usize) % STICKER_NAMES.len();
        let seed_counter = u32::from_le_bytes([raw[26], raw[27], raw[28], raw[29]]);

        let mut off = 30usize;
        let Some(meadow) = Self::decode_meadow(raw, &mut off) else {
            return false;
        };
        let Some(&undo_cnt) = raw.get(off) else {
            return false;
        };
        off += 1;
        if undo_cnt > 64 {
            return false;
        }
        let mut undo_stack = Vec::with_capacity(usize::from(undo_cnt));
        for _ in 0..undo_cnt {
            let Some(st) = Self::decode_meadow(raw, &mut off) else {
                return false;
            };
            undo_stack.push(st);
        }

        let Some(mem_bytes) = raw.get(off..off + 28) else {
            return false;
        };
        let mut cards = [0u8; 16];
        cards.copy_from_slice(&mem_bytes[0..16]);
        if cards.iter().any(|&c| c >= 8) {
            return false;
        }
        let rev_mask = u16::from_le_bytes([mem_bytes[16], mem_bytes[17]]);
        let match_mask = u16::from_le_bytes([mem_bytes[18], mem_bytes[19]]);
        let mut revealed = [false; 16];
        let mut matched = [false; 16];
        for i in 0..16 {
            revealed[i] = (rev_mask & (1 << i)) != 0;
            matched[i] = (match_mask & (1 << i)) != 0;
        }
        let cursor = (mem_bytes[20] as usize) % 16;
        let first_pick = if mem_bytes[21] < 16 {
            Some(mem_bytes[21] as usize)
        } else {
            None
        };
        let second_pick = if mem_bytes[22] < 16 {
            Some(mem_bytes[22] as usize)
        } else {
            None
        };
        let moves = u16::from_le_bytes([mem_bytes[23], mem_bytes[24]]);
        let two_player = (mem_bytes[25] & 1) != 0;
        let turn_p2 = (mem_bytes[25] & 2) != 0;
        let score_p1 = mem_bytes[26];
        let score_p2 = mem_bytes[27];

        self.progress = progress;
        self.tab = tab;
        self.den_cursor = den_cursor;
        self.mood = mood;
        self.level_idx = level_idx;
        self.sticker_cursor = sticker_cursor;
        self.seed_counter = seed_counter;
        self.meadow = meadow;
        self.undo_stack = undo_stack;
        self.hint_dir = None;
        self.status_banner.clear();
        self.memory = MemoryGame {
            cards,
            revealed,
            matched,
            cursor,
            first_pick,
            second_pick,
            moves,
            two_player,
            turn_p2,
            score_p1,
            score_p2,
        };
        true
    }
}

trait CardDotHelper {
    fn draw_circle_dots_card(&mut self, cx: i32, cy: i32);
}
impl<'a> CardDotHelper for Canvas<'a> {
    fn draw_circle_dots_card(&mut self, cx: i32, cy: i32) {
        self.stroke_circle(cx, cy, 16, 2, Tone::Paper);
        self.fill_circle(cx, cy, 5, Tone::Paper);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_25_starberry_meadow_levels_are_bfs_solvable() {
        for (idx, lvl) in MEADOW_LEVELS.iter().enumerate() {
            let start = MeadowState::from_level(idx);
            let sol = solve_meadow_bfs(&start, 15_000);
            assert!(
                sol.is_some(),
                "Level {} ({}) is NOT solvable!",
                idx + 1,
                lvl.name_en
            );
            let steps = sol.unwrap();
            assert!(
                !steps.is_empty() && steps.len() <= 60,
                "Level {} unexpected path length {}",
                idx + 1,
                steps.len()
            );
        }
    }

    #[test]
    fn replay_all_25_bfs_solutions_and_undo_stack() {
        for idx in 0..MEADOW_LEVELS.len() {
            let start = MeadowState::from_level(idx);
            let sol = solve_meadow_bfs(&start, 15_000).unwrap();
            let mut app = PocketPipApp::new(PipProgress::default());
            app.tab = PipTab::Meadow;
            app.level_idx = idx;
            app.meadow = start.clone();
            app.undo_stack.clear();

            for &dir in &sol {
                let btn = match dir {
                    Dir::Up => Button::Up,
                    Dir::Down => Button::Down,
                    Dir::Left => Button::Left,
                    Dir::Right => Button::Right,
                };
                app.handle_button(btn, false);
            }
            assert!(app.meadow.is_won(), "Replaying solution failed on level {}", idx + 1);

            // Undo every step via Confirm -> restores exact initial board state
            if !sol.is_empty() && idx + 1 < MEADOW_LEVELS.len() {
                // On winning the last move, Confirm may advance if kept on same level; test undo explicitly on level 0
            }
        }

        // Verify undo stack restores exact state on Level 0
        let mut app = PocketPipApp::new(PipProgress::default());
        app.tab = PipTab::Meadow;
        let orig = MeadowState::from_level(0);
        app.meadow = orig.clone();
        let sol = solve_meadow_bfs(&orig, 15_000).unwrap();
        let move_count = (sol.len() - 1).max(1);
        for &dir in &sol[..move_count] {
            let btn = match dir {
                Dir::Up => Button::Up,
                Dir::Down => Button::Down,
                Dir::Left => Button::Left,
                Dir::Right => Button::Right,
            };
            app.handle_button(btn, true);
        }
        assert_eq!(app.undo_stack.len(), move_count);
        for _ in 0..move_count {
            app.handle_button(Button::Back, true);
        }
        assert_eq!(app.meadow.pip, orig.pip);
        assert_eq!(app.meadow.acorns, orig.acorns);
        assert_eq!(app.meadow.berries, orig.berries);
    }

    #[test]
    fn memory_blossom_1p_and_2p_complete_games() {
        for two_player in [false, true] {
            let mut mem = MemoryGame::new(if two_player { 99 } else { 42 }, two_player);
            // Match every pair by known card value cleanly in ascending card id order
            for val in 0..8u8 {
                let idxs: Vec<usize> = mem
                    .cards
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &c)| if c == val { Some(i) } else { None })
                    .collect();
                assert_eq!(idxs.len(), 2);
                mem.cursor = idxs[0];
                assert!(mem.on_confirm().is_none());
                mem.cursor = idxs[1];
                let res = mem.on_confirm();
                if val == 7 {
                    assert_eq!(res, Some(true));
                } else {
                    assert!(res.is_none());
                }
            }
            assert!(mem.is_finished());
            assert_eq!(mem.score_p1 + mem.score_p2, 8);
        }
    }

    #[test]
    fn pocket_pip_encode_decode_preserves_meadow_undo_and_memory() {
        let mut pip = PocketPipApp::new(PipProgress::default());
        pip.tab = PipTab::Meadow;
        pip.load_level(1); // Level 2 has acorn + stream + starberries
        let sol = solve_meadow_bfs(&pip.meadow, 15_000).unwrap();
        let half = (sol.len() / 2).max(1);
        for &dir in &sol[..half] {
            let btn = match dir {
                Dir::Up => Button::Up,
                Dir::Down => Button::Down,
                Dir::Left => Button::Left,
                Dir::Right => Button::Right,
            };
            pip.handle_button(btn, false);
        }
        // Also put Memory in mid-2P play
        pip.memory = MemoryGame::new(77, true);
        let pair0: Vec<usize> = pip
            .memory
            .cards
            .iter()
            .enumerate()
            .filter_map(|(i, &c)| if c == 0 { Some(i) } else { None })
            .collect();
        pip.memory.cursor = pair0[0];
        pip.memory.on_confirm();
        pip.memory.cursor = pair0[1];
        pip.memory.on_confirm();

        let encoded = pip.encode_state();
        let mut restored = PocketPipApp::new(PipProgress::default());
        assert!(restored.decode_state(&encoded));
        assert_eq!(restored.tab, PipTab::Meadow);
        assert_eq!(restored.level_idx, 1);
        assert_eq!(restored.meadow, pip.meadow);
        assert_eq!(restored.undo_stack.len(), pip.undo_stack.len());
        assert_eq!(restored.memory.cards, pip.memory.cards);
        assert_eq!(restored.memory.matched, pip.memory.matched);
        assert!(restored.memory.two_player);
        assert_eq!(restored.memory.score_p1, 1);

        // Complete the rest of the Meadow puzzle from restored state!
        for &dir in &sol[half..] {
            let btn = match dir {
                Dir::Up => Button::Up,
                Dir::Down => Button::Down,
                Dir::Left => Button::Left,
                Dir::Right => Button::Right,
            };
            restored.handle_button(btn, false);
        }
        assert!(restored.meadow.is_won());
    }
}
