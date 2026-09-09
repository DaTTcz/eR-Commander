// eR Commander - Dual-pane file manager
// Copyright (C) 2025 David Trubka (DaTT.cz)
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// https://www.gnu.org/licenses/gpl-3.0.html

// Na Windows skryjeme konzolové okno - říkáme linkerovi že jde o GUI aplikaci.
// Bez tohoto atributu Windows automaticky otevře černé cmd okno.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use eframe::egui;
use notify::{RecommendedWatcher, RecursiveMode, Watcher, Event as FsEvent};
use regex::Regex;
use serde_json;
use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::SystemTime;
use walkdir::WalkDir;
use chrono::{DateTime, Local};

// Info pro okno "O programu" - klidně si uprav podle sebe.
const APP_AUTHOR:  &str = "David Trubka";
const APP_CONTACT: &str = "DaTT.cz";
// GitHub repo pro auto-update – uprav na svůj vlastní repo
const GITHUB_REPO: &str = "DaTTcz/eR-Commander";

// =====================================================================
// Datové struktury
// =====================================================================

#[derive(Clone)]
struct FileEntry {
    name: String,
    ext: String,
    is_dir: bool,
    size: u64,
    is_archive: bool,
    modified: Option<SystemTime>,
    readonly: bool,
    /// Rekurzivní velikost složky (None = dosud nepočítáno, Some(n) = hotovo).
    /// Pro soubory se ignoruje, používá se `size`.
    dir_size: Option<u64>,
}

impl FileEntry {
    fn modified_str(&self) -> String {
        match self.modified {
            Some(t) => {
                let dt: DateTime<Local> = t.into();
                dt.format("%d.%m.%Y %H:%M").to_string()
            }
            None => String::new(),
        }
    }

    fn attr_str(&self) -> &'static str {
        if self.readonly { "R" } else { "" }
    }

    /// Vrátí velikost pro zobrazení – pro soubory `size`, pro složky
    /// `dir_size` pokud je vypočtena, jinak 0.
    fn effective_size(&self) -> u64 {
        if self.is_dir { self.dir_size.unwrap_or(0) } else { self.size }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SortColumn { Name, Ext, Size, Date, Attr }

#[derive(Clone, Copy, PartialEq)]
enum SortDir { Asc, Desc }

#[derive(Clone)]
enum StatusMsg {
    Info(String),    // zelená  – úspěch, informace
    Warn(String),    // žlutá   – varování
    Error(String),   // červená – chyba
}

impl StatusMsg {
    fn text(&self) -> &str {
        match self { Self::Info(s) | Self::Warn(s) | Self::Error(s) => s }
    }
    fn color(&self) -> egui::Color32 {
        match self {
            Self::Info(_)  => egui::Color32::from_rgb(100, 220, 120),
            Self::Warn(_)  => egui::Color32::from_rgb(255, 200, 50),
            Self::Error(_) => egui::Color32::from_rgb(255, 70, 70),
        }
    }
}

/// Chování řazení složek
#[derive(Clone, Copy, PartialEq)]
enum DirSort {
    Mixed,      // složky a soubory smíchány dle zvoleného sloupce
    FirstByCol, // složky vždy nahoře, mezi sebou dle zvoleného sloupce
    FirstByName,// složky vždy nahoře, mezi sebou vždy dle jména
}

#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
enum ActivePanel {
    Left,
    Right,
}

/// Kam se panel "dívá", pokud je právě uvnitř otevřeného ZIP archivu
/// (procházíme obsah archivu, aniž bychom ho rozbalili na disk).
#[derive(Clone)]
struct ArchiveLocation {
    archive_path: PathBuf,
    /// Cesta uvnitř archivu, vždy končí "/" nebo je prázdná (kořen archivu).
    internal_dir: String,
}

struct Panel {
    current_path: PathBuf,
    entries: Vec<FileEntry>,
    selected: Vec<usize>, // indexy vybraných položek v `entries`
    cursor: usize,         // index aktuálně "podsvícené" položky (klávesová navigace)
    drives: Vec<String>,  // dostupné disky (na Windows C:\, D:\...)
    path_input: String,   // textové pole pro ruční zadání cesty (UNC apod.)
    path_focused: bool,    // má adresní řádka právě fokus? (zjišťováno každý frame)
    dir_size_needed: bool,
    cursor_moved: bool,
    show_hidden: bool,
    inline_rename_idx: Option<usize>,
    inline_rename_buf: String, // bylo kliknuto na složku? → spustit výpočet velikosti
    sort_col: SortColumn,
    sort_dir: SortDir,
    dir_sort: DirSort,
    archive_location: Option<ArchiveLocation>,
    error: Option<String>,
}

impl Panel {
    fn new(path: PathBuf) -> Self {
        let mut panel = Self {
            path_input: path.to_string_lossy().into_owned(),
            current_path: path,
            entries: Vec::new(),
            selected: Vec::new(),
            cursor: 0,
            drives: available_drives(),
            path_focused: false,
            dir_size_needed: false,
            cursor_moved: false,
            show_hidden: false,
            inline_rename_idx: None,
            inline_rename_buf: String::new(),
            sort_col: SortColumn::Name,
            sort_dir: SortDir::Asc,
            dir_sort: DirSort::FirstByCol,
            archive_location: None,
            error: None,
        };
        panel.refresh();
        panel
    }

    /// Lze jít o úroveň výš? (uvnitř archivu vždy ano, na disku jen pokud
    /// nejsme přímo v kořeni disku).
    fn can_go_up(&self) -> bool {
        self.archive_location.is_some() || self.current_path.parent().is_some()
    }

    fn refresh(&mut self) {
        self.entries.clear();
        self.selected.clear();
        self.error = None;

        if let Some(loc) = self.archive_location.clone() {
            match list_zip_dir(&loc.archive_path, &loc.internal_dir) {
                Ok(entries) => self.entries = entries,
                Err(e) => self.error = Some(e),
            }
            self.path_input = format!(
                "{}::/{}",
                loc.archive_path.display(),
                loc.internal_dir
            );
        } else {
            if let Ok(read_dir) = fs::read_dir(&self.current_path) {
                let mut items: Vec<FileEntry> = read_dir
                    .flatten()
                    .filter_map(|entry| {
                        let metadata = entry.metadata().ok()?;
                        let name = entry.file_name().to_string_lossy().into_owned();

                        // Filtrování skrytých/systémových souborů
                        if !self.show_hidden {
                            if name.starts_with('.') { return None; }
                            #[cfg(windows)] {
                                use std::os::windows::fs::MetadataExt;
                                const HIDDEN: u32 = 0x2;
                                const SYSTEM: u32 = 0x4;
                                if metadata.file_attributes() & (HIDDEN | SYSTEM) != 0 { return None; }
                            }
                        }

                        let ext = if metadata.is_dir() {
                            String::new()
                        } else {
                            Path::new(&name)
                                .extension()
                                .map(|e| e.to_string_lossy().to_lowercase())
                                .unwrap_or_default()
                                .to_string()
                        };
                        let readonly = metadata.permissions().readonly();
                        let modified = metadata.modified().ok();
                        Some(FileEntry {
                            is_archive: matches!(ext.as_str(), "zip" | "rar" | "7z"),
                            name,
                            ext,
                            is_dir: metadata.is_dir(),
                            size: metadata.len(),
                            modified,
                            readonly,
                            dir_size: None,
                        })
                    })
                    .collect();

                sort_entries_by(&mut items, self.sort_col, self.sort_dir, self.dir_sort);
                self.entries = items;
            }

            let p = self.current_path.to_string_lossy().into_owned();
            // Na Windows root disk C: -> C:\ (bez toho se nevejdeme zpět)
            self.path_input = if p.ends_with(':') { format!("{}\\", p) } else { p };
        }

        // Kurzor musí zůstat v platném rozsahu i po smazání/přesunu/navigaci
        // (počítáme i s řádkem ".." na začátku, pokud existuje).
        let total = self.entries.len() + if self.can_go_up() { 1 } else { 0 };
        self.cursor = self.cursor.min(total.saturating_sub(1));
    }

    /// Vstoupí do podadresáře - buď skutečného na disku, nebo (pokud jsme
    /// zrovna uvnitř archivu) do podsložky uvnitř ZIPu.
    fn enter(&mut self, name: &str) {
        if let Some(loc) = &mut self.archive_location {
            loc.internal_dir.push_str(name);
            loc.internal_dir.push('/');
        } else {
            self.current_path.push(name);
        }
        self.refresh();
    }

    /// Otevře ZIP archiv jako procházitelný "adresář".
    fn enter_zip_archive(&mut self, archive_path: PathBuf) {
        self.archive_location = Some(ArchiveLocation {
            archive_path,
            internal_dir: String::new(),
        });
        self.refresh();
    }

    fn go_up(&mut self) {
        if let Some(loc) = &mut self.archive_location {
            if loc.internal_dir.is_empty() {
                // Jsme v kořeni archivu - vrátíme se do reálné složky
                // a nastavíme kurzor na ZIP soubor
                let zip_name = self.current_path.file_name()
                    .unwrap_or_default().to_string_lossy().to_string();
                self.archive_location = None;
                self.refresh();
                // Kurzor na ZIP soubor
                if let Some(idx) = self.entries.iter().position(|e| e.name == zip_name) {
                    self.cursor = idx + self.up_offset();
                    self.cursor_moved = true;
                }
            } else {
                let trimmed = loc.internal_dir.trim_end_matches('/');
                let came_from = match trimmed.rfind('/') {
                    Some(pos) => {
                        let name = trimmed[pos+1..].to_string();
                        loc.internal_dir = format!("{}/", &trimmed[..pos]);
                        name
                    }
                    None => {
                        let name = trimmed.to_string();
                        loc.internal_dir.clear();
                        name
                    }
                };
                self.refresh();
                if let Some(idx) = self.entries.iter().position(|e| e.name == came_from) {
                    self.cursor = idx + self.up_offset();
                    self.cursor_moved = true;
                }
            }
        } else {
            // Pamatujeme si jméno aktuální složky před go_up
            let came_from = self.current_path.file_name()
                .unwrap_or_default().to_string_lossy().to_string();
            if self.current_path.pop() {
                self.refresh();
                // Kurzor na složku ze které jsme vyšli
                if !came_from.is_empty() {
                    if let Some(idx) = self.entries.iter().position(|e| e.name == came_from) {
                        self.cursor = idx + self.up_offset();
                        self.cursor_moved = true;
                    }
                }
            }
        }
    }

    /// Přejde na cestu zadanou ručně v textovém poli (podporuje UNC cesty
    /// typu \\NAS\share\slozka i běžné lokální cesty). Funguje jen mimo archiv.
    fn navigate_to_input(&mut self) -> Result<(), String> {
        let candidate = PathBuf::from(self.path_input.trim());
        if !candidate.exists() {
            return Err(format!(
                "Cesta neexistuje nebo není dostupná: {}",
                candidate.display()
            ));
        }
        if !candidate.is_dir() {
            return Err("Zadaná cesta není adresář.".to_string());
        }
        self.archive_location = None;
        self.current_path = candidate;
        self.refresh();
        Ok(())
    }

    /// Plné cesty na disku k vybraným položkám (jen mimo archiv).
    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .map(|e| self.current_path.join(&e.name))
            .collect()
    }

    /// Vrátí vybrané soubory, nebo pokud nic není vybráno, soubor pod
    /// kurzorem. Stejné chování jako Total Commander.
    fn effective_paths(&self) -> Vec<PathBuf> {
        let selected = self.selected_paths();
        if !selected.is_empty() { return selected; }
        if let Some(idx) = self.entry_index_at_cursor() {
            if let Some(entry) = self.entries.get(idx) {
                return vec![self.current_path.join(&entry.name)];
            }
        }
        Vec::new()
    }

    /// Vrátí vybraná jména, nebo pokud nic není vybráno, jméno pod kurzorem.
    fn effective_names(&self) -> Vec<String> {
        let selected = self.selected_names();
        if !selected.is_empty() { return selected; }
        if let Some(idx) = self.entry_index_at_cursor() {
            if let Some(entry) = self.entries.get(idx) {
                return vec![entry.name.clone()];
            }
        }
        Vec::new()
    }

    /// Jen jména vybraných položek (funguje jak mimo, tak uvnitř archivu).
    fn selected_names(&self) -> Vec<String> {
        self.selected
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .map(|e| e.name.clone())
            .collect()
    }

    /// Kolik "řádků" má panel pro účely klávesové navigace - běžné
    /// položky plus případně řádek ".." navíc.
    fn total_rows(&self) -> usize {
        self.entries.len() + if self.can_go_up() { 1 } else { 0 }
    }

    /// Kolik pozic na začátku zabírá ".." (0 nebo 1) - o tolik je posunutý
    /// index v `entries` oproti pozici kurzoru.
    fn up_offset(&self) -> usize {
        if self.can_go_up() { 1 } else { 0 }
    }

    /// Na jaký index v `entries` aktuálně ukazuje kurzor - `None`, pokud
    /// kurzor stojí na řádku "..".
    fn entry_index_at_cursor(&self) -> Option<usize> {
        self.cursor.checked_sub(self.up_offset())
    }

    /// Posune kurzor o `delta` řádků, s omezením na hranice seznamu
    /// (včetně řádku ".." na začátku, pokud existuje).
    fn move_cursor(&mut self, delta: isize) {
        let total = self.total_rows();
        if total == 0 { return; }
        let new_pos = (self.cursor as isize + delta).clamp(0, total as isize - 1);
        self.cursor = new_pos as usize;
        self.cursor_moved = true;
    }

    fn move_cursor_to_start(&mut self) {
        self.cursor = 0;
        self.cursor_moved = true;
    }

    fn move_cursor_to_end(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.cursor = total - 1;
            self.cursor_moved = true;
        }
    }

    /// Mezerník - označí/odznačí položku pod kurzorem a posune kurzor dál
    /// (stejné chování jako ve Far/Total Commanderu). Na řádku ".." nic
    /// neoznačuje, jen posune kurzor dál.
    fn toggle_selection_at_cursor(&mut self) {
        if let Some(idx) = self.entry_index_at_cursor() {
            if self.selected.contains(&idx) {
                self.selected.retain(|&x| x != idx);
            } else {
                self.selected.push(idx);
            }
        }
        // Záměrně NEposouváme kurzor - mezerník jen označí/odznačí
        // bez pohybu, jako ve Windows Průzkumníku.
    }

    /// Enter na položce pod kurzorem: na ".." jde o úroveň výš, jinak
    /// vstoupí do adresáře / otevře ZIP jako průchozí složku / u běžného
    /// souboru vrátí akci pro otevření v systémové aplikaci.
    fn activate_cursor(&mut self) -> Option<CursorAction> {
        if self.entry_index_at_cursor().is_none() {
            // Kurzor je na ".." - o úroveň výš.
            self.go_up();
            return None;
        }

        let idx = self.entry_index_at_cursor()?;
        let entry = self.entries.get(idx)?.clone();

        if entry.is_dir {
            self.enter(&entry.name);
            return None;
        }

        if self.archive_location.is_some() {
            // Uvnitř archivu Enter na souboru nic nedělá - rozbalení jde přes F5.
            return None;
        }

        let full_path = self.current_path.join(&entry.name);
        if entry.is_archive {
            let ext = full_path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if ext == "zip" {
                self.enter_zip_archive(full_path);
                return None;
            }
        }
        Some(CursorAction::OpenExternal(full_path))
    }
}

/// Akce, kterou musí zpracovat volající (FileManagerApp), protože jde
/// o efekt mimo samotný Panel (spuštění externí aplikace).
enum CursorAction {
    OpenExternal(PathBuf),
}

/// Akce z panelu, které musí zpracovat FileManagerApp (mají dopad mimo
/// samotný Panel, typicky otevření sdíleného dialogu).
enum PanelUiAction {
    OpenBookmarks,
}

/// Akce z kontextového menu - vrací se z render_panel do update()
enum ContextAction {
    Copy,
    Move,
    Rename,
    Delete,
    NewDir,
    NewFile,
    Open(PathBuf),
    Edit(PathBuf),
    SelectAll,
    DeselectAll,
    Hash,
}

fn sort_entries(items: &mut [FileEntry]) {
    sort_entries_by(items, SortColumn::Name, SortDir::Asc, DirSort::FirstByCol);
}

fn sort_entries_by(items: &mut [FileEntry], col: SortColumn, dir: SortDir, dir_sort: DirSort) {
    items.sort_by(|a, b| {
        // Složky nahoře?
        if dir_sort != DirSort::Mixed {
            match (a.is_dir, b.is_dir) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
        }

        // Pokud jsou oba stejného typu (nebo Mixed), řadíme dle sloupce
        // Ale pokud jsou oba složky a je FirstByName, řadíme vždy dle jména
        let both_dirs = a.is_dir && b.is_dir;
        let effective_col = if both_dirs && dir_sort == DirSort::FirstByName {
            SortColumn::Name
        } else {
            col
        };

        let ord = match effective_col {
            SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortColumn::Ext  => a.ext.to_lowercase().cmp(&b.ext.to_lowercase())
                                 .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            SortColumn::Size => a.size.cmp(&b.size),
            SortColumn::Date => a.modified.cmp(&b.modified),
            SortColumn::Attr => a.attr_str().cmp(b.attr_str()),
        };

        // Směr řazení se aplikuje vždy, kromě složek v FirstByName módu
        if both_dirs && dir_sort == DirSort::FirstByName {
            ord // složky vždy Asc bez ohledu na dir
        } else if dir == SortDir::Desc {
            ord.reverse()
        } else {
            ord
        }
    });
}

/// Vrátí seznam dostupných disků na Windows (C:\, D:\, mapované síťové disky...).
/// Na Linuxu/macOS vrátí jen kořen "/", protože tam koncept "disků" neexistuje.
fn available_drives() -> Vec<String> {
    if cfg!(windows) {
        (b'A'..=b'Z')
            .filter_map(|letter| {
                let drive = format!("{}:\\", letter as char);
                if Path::new(&drive).exists() {
                    Some(drive)
                } else {
                    None
                }
            })
            .collect()
    } else {
        vec!["/".to_string()]
    }
}

fn dirs_home() -> PathBuf {
    if let Some(path) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("HOME") {
        return PathBuf::from(path);
    }
    if cfg!(windows) {
        PathBuf::from("C:\\")
    } else {
        PathBuf::from("/")
    }
}

/// Vrátí (volné bajty, celkem bajtů) pro disk, na kterém leží `path`.
/// Na Windows voláno přímo přes WinAPI (žádná další závislost navíc).
/// Na jiných platformách zatím nepodporováno (appka to prostě nezobrazí).
#[cfg(windows)]
fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lp_directory_name: *const u16,
            lp_free_bytes_available: *mut u64,
            lp_total_number_of_bytes: *mut u64,
            lp_total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut free_available: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_free: u64 = 0;

    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_available,
            &mut total_bytes,
            &mut total_free,
        )
    };

    if ok != 0 {
        Some((free_available, total_bytes))
    } else {
        None
    }
}

#[cfg(not(windows))]
fn disk_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

// =====================================================================
// Perzistence stavu mezi spuštěními (naposledy otevřené složky, záložky)
// =====================================================================

fn state_file_path() -> PathBuf {
    dirs_home().join(".er_commander_state.txt")
}

/// Načte uložený stav z minulého spuštění. Neplatné/neexistující cesty
/// se tiše přeskočí, ať appka nikdy neskončí na nesmyslné složce.
/// Záložka může být jednoduchá (jedna cesta) nebo dvojitá (levý+pravý panel).
#[derive(Clone)]
struct Bookmark {
    name:  String,
    left:  PathBuf,
    right: Option<PathBuf>,  // None = jednoduchá záložka
}

fn load_state() -> (Option<PathBuf>, Option<PathBuf>, Vec<Bookmark>,
                    Option<SortColumn>, Option<SortDir>, Option<SortColumn>, Option<SortDir>,
                    bool) {
    let mut left        = None;
    let mut right       = None;
    let mut bookmarks   = Vec::new();
    let mut sort_l_col  = None;
    let mut sort_l_dir  = None;
    let mut sort_r_col  = None;
    let mut sort_r_dir  = None;
    let mut dark_mode   = true; // výchozí tmavé téma

    fn valid_path(s: &str) -> Option<PathBuf> {
        let p = PathBuf::from(s.trim());
        if s.starts_with("\\\\") || p.is_dir() { Some(p) } else { None }
    }
    fn parse_sort_col(s: &str) -> Option<SortColumn> {
        match s.trim() {
            "Name" => Some(SortColumn::Name), "Ext"  => Some(SortColumn::Ext),
            "Size" => Some(SortColumn::Size), "Date" => Some(SortColumn::Date),
            "Attr" => Some(SortColumn::Attr), _ => None,
        }
    }
    fn parse_sort_dir(s: &str) -> Option<SortDir> {
        match s.trim() { "Asc" => Some(SortDir::Asc), "Desc" => Some(SortDir::Desc), _ => None }
    }

    if let Ok(content) = fs::read_to_string(state_file_path()) {
        let mut bm_name: Option<String> = None;
        let mut bm_left: Option<PathBuf> = None;

        for line in content.lines() {
            if let Some(v) = line.strip_prefix("LEFT=")         { left = valid_path(v); }
            else if let Some(v) = line.strip_prefix("RIGHT=")   { right = valid_path(v); }
            else if let Some(v) = line.strip_prefix("SORT_L_COL=") { sort_l_col = parse_sort_col(v); }
            else if let Some(v) = line.strip_prefix("SORT_L_DIR=") { sort_l_dir = parse_sort_dir(v); }
            else if let Some(v) = line.strip_prefix("SORT_R_COL=") { sort_r_col = parse_sort_col(v); }
            else if let Some(v) = line.strip_prefix("SORT_R_DIR=") { sort_r_dir = parse_sort_dir(v); }
            else if let Some(v) = line.strip_prefix("DARK_MODE=")  { dark_mode = v.trim() == "true"; }
            else if let Some(v) = line.strip_prefix("DIR_SORT=") {
                // dir_sort načteme přímo do FileManagerApp v Default
                let _ = v; // zpracováno níže
            }
            else if let Some(v) = line.strip_prefix("BM_NAME=")  { bm_name = Some(v.trim().to_string()); }
            else if let Some(v) = line.strip_prefix("BM_LEFT=")  { bm_left = valid_path(v); }
            else if let Some(v) = line.strip_prefix("BM_RIGHT=") {
                if let (Some(name), Some(bl)) = (bm_name.take(), bm_left.take()) {
                    bookmarks.push(Bookmark { name, left: bl, right: valid_path(v) });
                }
            }
            else if let Some(v) = line.strip_prefix("BOOKMARK=") {
                if let Some(p) = valid_path(v) {
                    bookmarks.push(Bookmark {
                        name: p.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                        left: p, right: None,
                    });
                }
            }
        }
    }

    (left, right, bookmarks, sort_l_col, sort_l_dir, sort_r_col, sort_r_dir, dark_mode)
}

// =====================================================================
// Operace na pozadí (kopírování / přesun / mazání) + progress
// =====================================================================

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

// Stav vlákna operace: 0=běží, 1=pauza, 2=stop
type OpFlag = Arc<AtomicU8>;
const OP_RUN:   u8 = 0;
const OP_PAUSE: u8 = 1;
const OP_STOP:  u8 = 2;

#[derive(Clone, Copy, PartialEq)]
enum OpKind {
    Copy,
    Move,
    Delete,
}

/// Zpráva z operačního vlákna. thread_id identifikuje konkrétní rayon slot (0-3).
enum OpMsg {
    SlotStart {
        thread_id: usize,
        file: String,
        file_size: u64,
    },
    SlotProgress {
        thread_id: usize,
        bytes_copied: u64,
    },
    SlotDone {
        thread_id: usize,
    },
    /// Celkový progress (done/total souborů, bytes celkem)
    Progress {
        done: usize,
        total: usize,
        bytes_done: u64,
        bytes_total: u64,
    },
    Skipped(()),
    Finished,
    Error(String),
    /// Chyba při zpracování souboru - čekáme na odpověď uživatele
    ErrorWait { file: String, error: String },
}

// Odpovědi na chybu operace
const ERR_WAIT:   u8 = 0;
const ERR_RETRY:  u8 = 1;
const ERR_SKIP:   u8 = 2;
const ERR_CANCEL: u8 = 3;

/// Stav jednoho aktivního souboru v progress dialogu
#[derive(Clone)]
struct FileSlot {
    file: String,
    size: u64,
    copied: u64,
}

// =====================================================================
// Kontrolní součet (MD5 / SHA-256)
// =====================================================================

enum HashMsg {
    Progress { done: usize, total: usize, current: String },
    Result { name: String, md5: String, sha256: String },
    Error { name: String, msg: String },
    Finished,
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Spočítá MD5 i SHA-256 pro dané soubory na pozadí, jedním průchodem
/// (čte se každý soubor jen jednou, oba hashe se počítají zároveň).
fn spawn_hash_calc(files: Vec<PathBuf>, flag: OpFlag) -> Receiver<HashMsg> {
    use sha2::{Sha256, Digest};
    use md5::Md5;
    use std::io::Read;

    let (tx, rx) = channel();
    thread::spawn(move || {
        let total = files.len();
        for (i, path) in files.iter().enumerate() {
            if flag.load(Ordering::Relaxed) == OP_STOP { break; }
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let _ = tx.send(HashMsg::Progress { done: i, total, current: name.clone() });

            let result: std::io::Result<(String, String)> = (|| {
                let mut f = File::open(path)?;
                let mut md5_hasher = Md5::new();
                let mut sha_hasher  = Sha256::new();
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    if flag.load(Ordering::Relaxed) == OP_STOP { break; }
                    let n = f.read(&mut buf)?;
                    if n == 0 { break; }
                    md5_hasher.update(&buf[..n]);
                    sha_hasher.update(&buf[..n]);
                }
                Ok((bytes_to_hex(&md5_hasher.finalize()), bytes_to_hex(&sha_hasher.finalize())))
            })();

            match result {
                Ok((md5, sha256)) => { let _ = tx.send(HashMsg::Result { name, md5, sha256 }); }
                Err(e) => { let _ = tx.send(HashMsg::Error { name, msg: e.to_string() }); }
            }
        }
        let _ = tx.send(HashMsg::Finished);
    });
    rx
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

fn delete_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Spustí operaci na pozadí. Vrátí (Receiver, OpFlag) kde OpFlag
/// slouží pro pause/resume/stop z UI vlákna.
fn spawn_file_op(
    kind: OpKind,
    files: Vec<PathBuf>,
    dst_dir: Option<PathBuf>,
    overwrite: bool,
    err_flag: Arc<AtomicU8>,
    dst_name: Option<String>,  // přejmenování při kopírování jednoho souboru
) -> (Receiver<OpMsg>, OpFlag) {
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use rayon::prelude::*;

    let (tx, rx) = channel();
    let flag: OpFlag = Arc::new(AtomicU8::new(OP_RUN));
    let flag_thread  = flag.clone();
    let err_flag_th  = Arc::new(err_flag);

    let bytes_total: u64 = files.iter().map(|p| {
        if p.is_file() { p.metadata().map(|m| m.len()).unwrap_or(0) } else { 0 }
    }).sum();

    thread::spawn(move || {
        let total        = files.len();
        let num_threads  = 4.min(total).max(1);
        let done_counter = Arc::new(AtomicUsize::new(0));
        let bytes_global = Arc::new(AtomicU64::new(0));
        let tx           = Arc::new(std::sync::Mutex::new(tx));
        let err_flag_th  = Arc::new(err_flag_th);

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());

        pool.install(|| {
            files.par_iter().enumerate().for_each(|(par_idx, src)| {
                // Každý rayon worker dostane pevné thread_id (0..num_threads)
                // Trik: par_idx mod num_threads dává konzistentní slot ID
                let thread_id = par_idx % num_threads;

                if flag_thread.load(Ordering::Relaxed) == OP_STOP { return; }
                while flag_thread.load(Ordering::Relaxed) == OP_PAUSE {
                    thread::sleep(std::time::Duration::from_millis(100));
                }
                if flag_thread.load(Ordering::Relaxed) == OP_STOP { return; }

                let file_name = src.file_name()
                    .unwrap_or_default().to_string_lossy().into_owned();
                // Cílový název - dst_name pro přejmenování, jinak původní jméno
                let target_name = dst_name.as_deref().unwrap_or(&file_name).to_string();
                let file_size = if src.is_file() {
                    src.metadata().map(|m| m.len()).unwrap_or(0)
                } else { 0 };

                if kind != OpKind::Delete {
                    let dst = dst_dir.as_ref().unwrap().join(&target_name);
                    if !overwrite && dst.exists() {
                        if let Ok(t) = tx.lock() { let _ = t.send(OpMsg::Skipped(())); }
                        return;
                    }
                }

                // Oznámíme start slotu
                if let Ok(t) = tx.lock() {
                    let _ = t.send(OpMsg::SlotStart {
                        thread_id,
                        file: target_name.clone(),
                        file_size,
                    });
                }

                let result: std::io::Result<()> = match kind {
                    OpKind::Copy => {
                        let dst = dst_dir.as_ref().unwrap().join(&target_name);
                        if src.is_dir() {
                            copy_dir_recursive(src, &dst)
                        } else {
                            // Vlastní copy loop v IIFE closure (vrací Result, ? funguje)
                            (|| -> std::io::Result<()> {
                                use std::io::{Read, Write};
                                let mut src_file = fs::File::open(src)?;
                                let mut dst_file = fs::File::create(&dst)?;
                                let mut buf = vec![0u8; 256 * 1024];
                                loop {
                                    if flag_thread.load(Ordering::Relaxed) == OP_STOP {
                                        break;
                                    }
                                    while flag_thread.load(Ordering::Relaxed) == OP_PAUSE {
                                        thread::sleep(std::time::Duration::from_millis(50));
                                    }
                                    let n = src_file.read(&mut buf)?;
                                    if n == 0 { break; }
                                    dst_file.write_all(&buf[..n])?;
                                    let bytes = bytes_global.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                                    if let Ok(t) = tx.lock() {
                                        let _ = t.send(OpMsg::SlotProgress {
                                            thread_id,
                                            bytes_copied: n as u64,
                                        });
                                        let done = done_counter.load(Ordering::Relaxed);
                                        let _ = t.send(OpMsg::Progress {
                                            done,
                                            total,
                                            bytes_done: bytes,
                                            bytes_total,
                                        });
                                    }
                                }
                                Ok(())
                            })()
                        }
                    }
                    OpKind::Move => {
                        let dst = dst_dir.as_ref().unwrap().join(&target_name);
                        // Zkusíme nejdřív rychlý rename (stejný disk)
                        match fs::rename(src, &dst) {
                            Ok(()) => {
                                // Rename je okamžitý - aktualizujeme bytes pro progress
                                let bytes = bytes_global.fetch_add(file_size, Ordering::Relaxed) + file_size;
                                if let Ok(t) = tx.lock() {
                                    let _ = t.send(OpMsg::Progress {
                                        done: done_counter.load(Ordering::Relaxed),
                                        total,
                                        bytes_done: bytes,
                                        bytes_total,
                                    });
                                }
                                Ok(())
                            },
                            Err(_) => {
                                // Mezidiskový přesun - IIFE aby ? fungovalo v for_each closure
                                (|| -> std::io::Result<()> {
                                    if src.is_dir() {
                                        copy_dir_recursive(src, &dst)?;
                                        fs::remove_dir_all(src)?;
                                    } else {
                                        use std::io::{Read, Write};
                                        let mut src_file = fs::File::open(src)?;
                                        let mut dst_file = fs::File::create(&dst)?;
                                        let mut buf = vec![0u8; 256 * 1024];
                                        loop {
                                            if flag_thread.load(Ordering::Relaxed) == OP_STOP { break; }
                                            while flag_thread.load(Ordering::Relaxed) == OP_PAUSE {
                                                thread::sleep(std::time::Duration::from_millis(50));
                                            }
                                            let n = src_file.read(&mut buf)?;
                                            if n == 0 { break; }
                                            dst_file.write_all(&buf[..n])?;
                                            let bytes = bytes_global.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                                            if let Ok(t) = tx.lock() {
                                                let _ = t.send(OpMsg::SlotProgress { thread_id, bytes_copied: n as u64 });
                                                let done = done_counter.load(Ordering::Relaxed);
                                                let _ = t.send(OpMsg::Progress { done, total, bytes_done: bytes, bytes_total });
                                            }
                                        }
                                        fs::remove_file(src)?;
                                    }
                                    Ok(())
                                })()
                            }
                        }
                    }
                    OpKind::Delete => delete_path(src),
                };

                if let Err(e) = result {
                    // Pošleme ErrorWait a čekáme na odpověď
                    err_flag_th.store(ERR_WAIT, Ordering::Relaxed);
                    if let Ok(t) = tx.lock() {
                        let _ = t.send(OpMsg::ErrorWait {
                            file: file_name.clone(),
                            error: e.to_string(),
                        });
                    }
                    loop {
                        thread::sleep(std::time::Duration::from_millis(50));
                        match err_flag_th.load(Ordering::Relaxed) {
                            ERR_RETRY  => break, // pokračujeme (přeskočíme)
                            ERR_SKIP   => { return; }
                            ERR_CANCEL => {
                                flag_thread.store(OP_STOP, Ordering::Relaxed);
                                return;
                            }
                            _ => {} // čekáme
                        }
                    }
                    return; // po retry přeskočíme
                }

                let done  = done_counter.fetch_add(1, Ordering::Relaxed) + 1;
                let bytes = bytes_global.load(Ordering::Relaxed);

                if let Ok(t) = tx.lock() {
                    let _ = t.send(OpMsg::SlotDone { thread_id });
                    let _ = t.send(OpMsg::Progress { done, total, bytes_done: bytes, bytes_total });
                }
            });
        });

        if flag_thread.load(Ordering::Relaxed) == OP_STOP {
            if let Ok(t) = tx.lock() {
                let _ = t.send(OpMsg::Error("Operace zastavena uživatelem.".to_string()));
            }
        } else if let Ok(t) = tx.lock() {
            let _ = t.send(OpMsg::Finished);
        }
    });

    (rx, flag)
}

// =====================================================================
// Práce se ZIP archivy (procházení jako adresář + extrakce)
// =====================================================================

/// Vylistuje "obsah adresáře" uvnitř ZIPu na dané interní cestě
/// (immediate children - podsložky i soubory), podobně jako fs::read_dir.
fn list_zip_dir(archive_path: &Path, internal_dir: &str) -> Result<Vec<FileEntry>, String> {
    let file = File::open(archive_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    let mut dirs_seen: HashSet<String> = HashSet::new();
    let mut entries = Vec::new();

    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().replace('\\', "/");

        if !name.starts_with(internal_dir) {
            continue;
        }
        let rest = &name[internal_dir.len()..];
        if rest.is_empty() {
            continue;
        }

        match rest.find('/') {
            Some(slash_pos) => {
                let dir_name = &rest[..slash_pos];
                if dirs_seen.insert(dir_name.to_string()) {
                    entries.push(FileEntry {
                        name: dir_name.to_string(),
                        ext: String::new(),
                        is_dir: true,
                        size: 0,
                        is_archive: false,
                        modified: None,
                        readonly: false,
                        dir_size: None,
                    });
                }
            }
            None => {
                let name = rest.to_string();
                let ext = Path::new(&name)
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default()
                    .to_string();
                entries.push(FileEntry {
                    is_archive: matches!(ext.as_str(), "zip" | "rar" | "7z"),
                    name,
                    ext,
                    is_dir: false,
                    size: entry.size(),
                    modified: None,
                    readonly: false,
                    dir_size: None,
                });
            }
        }
    }

    sort_entries(&mut entries);
    Ok(entries)
}

/// Rozbalí vybrané položky (soubory nebo celé podsložky) z dané interní
/// cesty v ZIPu do cílového adresáře na disku, se zachováním struktury.
/// Běží na pozadí ve vlastním vlákně stejně jako ostatní operace.
fn spawn_zip_pack(files: Vec<PathBuf>, zip_path: PathBuf) -> (Receiver<OpMsg>, OpFlag) {
    let (tx, rx) = channel();
    let flag: OpFlag = Arc::new(AtomicU8::new(OP_RUN));
    let flag_thread = flag.clone();
    thread::spawn(move || {
        let result = (|| -> std::io::Result<()> {
            let file = fs::File::create(&zip_path)?;
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let total = files.len();
            for (i, src) in files.iter().enumerate() {
                if flag_thread.load(Ordering::Relaxed) == OP_STOP { break; }
                let _ = tx.send(OpMsg::Progress { done: i, total, bytes_done: 0, bytes_total: 0 });
                let file_name = src.file_name().unwrap_or_default().to_string_lossy().to_string();
                if src.is_dir() {
                    add_dir_to_zip(&mut zip, src, src, &options)?;
                } else {
                    zip.start_file(&file_name, options)?;
                    let mut f = fs::File::open(src)?;
                    std::io::copy(&mut f, &mut zip)?;
                }
            }
            zip.finish()?;
            Ok(())
        })();
        match result {
            Ok(())  => { let _ = tx.send(OpMsg::Finished); }
            Err(e)  => { let _ = tx.send(OpMsg::Error(e.to_string())); }
        }
    });
    (rx, flag)
}

fn add_dir_to_zip(zip: &mut zip::ZipWriter<fs::File>, base: &Path, dir: &Path,
    options: &zip::write::FileOptions) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path  = entry.path();
        let rel   = path.strip_prefix(base).unwrap_or(&path);
        let name  = rel.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            zip.add_directory(&name, *options)?;
            add_dir_to_zip(zip, base, &path, options)?;
        } else {
            zip.start_file(&name, *options)?;
            let mut f = fs::File::open(&path)?;
            std::io::copy(&mut f, zip)?;
        }
    }
    Ok(())
}

fn spawn_zip_extract(
    archive_path: PathBuf,
    internal_dir: String,
    selected_names: Vec<String>,
    dst_dir: PathBuf,
    overwrite: bool,
    err_flag: Arc<AtomicU8>,
) -> (Receiver<OpMsg>, OpFlag) {
    let (tx, rx) = channel();
    let flag: OpFlag = Arc::new(AtomicU8::new(OP_RUN));
    let flag_thread = flag.clone();

    thread::spawn(move || {
        let file = match File::open(&archive_path) {
            Ok(f) => f,
            Err(e) => { let _ = tx.send(OpMsg::Error(e.to_string())); return; }
        };
        let mut archive = match zip::ZipArchive::new(file) {
            Ok(a) => a,
            Err(e) => { let _ = tx.send(OpMsg::Error(e.to_string())); return; }
        };
        let total = archive.len();
        let bytes_total: u64 = (0..total).filter_map(|i| {
            archive.by_index(i).ok().map(|e| e.size())
        }).sum();
        let mut bytes_done: u64 = 0;

        for i in 0..total {
            if flag_thread.load(Ordering::Relaxed) == OP_STOP { break; }
            while flag_thread.load(Ordering::Relaxed) == OP_PAUSE {
                thread::sleep(std::time::Duration::from_millis(100));
            }

            let mut entry = match archive.by_index(i) {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx.send(OpMsg::Error(e.to_string()));
                    continue;
                }
            };
            let name = entry.name().replace('\\', "/");
            if !name.starts_with(&internal_dir) { continue; }
            let rest = &name[internal_dir.len()..];
            if rest.is_empty() { continue; }
            let matched = selected_names.iter().any(|sel| {
                rest == sel.as_str() || rest.starts_with(&format!("{}/", sel))
            });
            if !matched { continue; }

            let out_path = dst_dir.join(rest);
            if entry.is_dir() {
                let _ = fs::create_dir_all(&out_path);
                continue;
            }
            if !overwrite && out_path.exists() {
                let _ = tx.send(OpMsg::Skipped(()));
                continue;
            }
            if let Some(parent) = out_path.parent() {
                let _ = fs::create_dir_all(parent);
            }

            let entry_size = entry.size();
            let file_name = rest.to_string();

            // Pokus o zápis - při chybě zobrazíme dialog
            let write_result = (|| -> std::io::Result<()> {
                let mut out_file = File::create(&out_path)?;
                std::io::copy(&mut entry, &mut out_file)?;
                Ok(())
            })();

            if let Err(e) = write_result {
                // Pošleme ErrorWait a čekáme na odpověď uživatele
                err_flag.store(ERR_WAIT, Ordering::Relaxed);
                let _ = tx.send(OpMsg::ErrorWait {
                    file: file_name,
                    error: e.to_string(),
                });
                loop {
                    thread::sleep(std::time::Duration::from_millis(50));
                    match err_flag.load(Ordering::Relaxed) {
                        ERR_RETRY | ERR_SKIP => { break; }
                        ERR_CANCEL => {
                            flag_thread.store(OP_STOP, Ordering::Relaxed);
                            let _ = tx.send(OpMsg::Finished);
                            return;
                        }
                        _ => {}
                    }
                }
                continue; // přeskočíme chybný soubor
            }

            bytes_done += entry_size;
            let _ = tx.send(OpMsg::Progress {
                done: i + 1,
                total,
                bytes_done,
                bytes_total,
            });
        }

        let _ = tx.send(OpMsg::Finished);
    });

    (rx, flag)
}

/// Otevře soubor v systémově asociované aplikaci (RAR/7z - protože pro ně
/// není spolehlivá čistě Rust knihovna, spoléháme na WinRAR/7-Zip apod.).
fn open_with_system_app(path: &Path) -> std::io::Result<()> {
    if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &path.display().to_string()])
            .spawn()
            .map(|_| ())
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(path).spawn().map(|_| ())
    } else {
        std::process::Command::new("xdg-open").arg(path).spawn().map(|_| ())
    }
}

/// Vrátí jména těch souborů/adresářů, které by v cílovém adresáři přepsaly
/// už existující položku (kontrola PŘED spuštěním operace na pozadí).
fn paths_conflicting(files: &[PathBuf], dst_dir: &Path) -> Vec<String> {
    files
        .iter()
        .filter_map(|src| {
            let name = src.file_name()?.to_string_lossy().into_owned();
            if dst_dir.join(&name).exists() {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

/// Totéž, ale pro položky vybrané uvnitř ZIP archivu (porovnává jen podle jména).
fn names_conflicting(names: &[String], dst_dir: &Path) -> Vec<String> {
    names
        .iter()
        .filter(|name| dst_dir.join(name).exists())
        .cloned()
        .collect()
}

// =====================================================================
// Hledání souborů (Alt+F7) - podle jména a/nebo textu uvnitř souborů
// =====================================================================

enum SearchMsg {
    Found(PathBuf),
    Scanned(usize),
    Finished,
}

/// Rekurzivně prohledá `root` a hledá soubory podle jména (podřetězec,
/// bez rozlišení velikosti písmen) a/nebo podle textu uvnitř souboru.
/// Obsahové hledání přeskakuje soubory nad 10 MB, ať appka nezamrzne na
/// velkých binárkách/videích.
fn spawn_search(root: PathBuf, name_pattern: String, content_pattern: String) -> Receiver<SearchMsg> {
    let (tx, rx) = channel();
    let name_lower = name_pattern.to_lowercase();
    let content_lower = content_pattern.to_lowercase();

    thread::spawn(move || {
        let mut scanned = 0usize;

        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            scanned += 1;
            if scanned % 200 == 0 {
                let _ = tx.send(SearchMsg::Scanned(scanned));
            }

            let file_name = entry.file_name().to_string_lossy().to_lowercase();
            if !name_lower.is_empty() && !file_name.contains(&name_lower) {
                continue;
            }

            if content_lower.is_empty() {
                let _ = tx.send(SearchMsg::Found(entry.path().to_path_buf()));
                continue;
            }

            let too_big = entry
                .metadata()
                .map(|m| m.len() > 10 * 1024 * 1024)
                .unwrap_or(true);
            if too_big {
                continue;
            }

            if let Ok(content) = fs::read_to_string(entry.path()) {
                if content.to_lowercase().contains(&content_lower) {
                    let _ = tx.send(SearchMsg::Found(entry.path().to_path_buf()));
                }
            }
        }

        let _ = tx.send(SearchMsg::Finished);
    });

    rx
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

// =====================================================================
// Hlavní aplikace
// =====================================================================

/// Operace čekající na rozhodnutí uživatele (přepsat / přeskočit existující).
enum PendingAction {
    FsOp { kind: OpKind, files: Vec<PathBuf>, dst: PathBuf },
    ZipExtract {
        archive_path: PathBuf,
        internal_dir: String,
        names: Vec<String>,
        dst: PathBuf,
    },
}

/// Vykreslí SVG ikonku. Každá ikonka MUSÍ mít unikátní `uri` (klíč pro cache egui).
/// Bez toho by egui cachoval první ikonku a zobrazoval ji pro všechny.
/// Unicode ikonky - fungují spolehlivě s jakýmkoliv egui fontem,
/// bez cache problémů nebo API zádrhelů při načítání SVG.
struct Icons;

impl Icons {
    // Emoji ikonky - fungují díky Segoe UI Emoji fontu načtenému v main()
    fn folder()   -> &'static str { "\u{1F4C1}" }  // 📁
    fn file()     -> &'static str { "\u{1F4C4}" }  // 📄
    fn archive()  -> &'static str { "\u{1F4E6}" }  // 📦
    fn up()       -> &'static str { "\u{2B06}"  }  // ⬆
    fn bookmark() -> &'static str { "\u{2605}"  }  // ★
}

#[derive(Clone, PartialEq)]
enum UpdateState {
    Idle,
    Checking,
    UpdateAvailable { version: String, url: String },
    Downloading,
    UpToDate,
}

#[derive(Clone)]
struct UpdateCheckResult {
    version: String,
    download_url: String,
}

/// Přípona release assetu odpovídající aktuální platformě.
/// Windows build se publikuje jako .zip (obsahuje .exe),
/// Linux build jako .tar.gz (obsahuje binárku).
fn platform_release_asset_suffix() -> &'static str {
    if cfg!(windows) { ".zip" } else { ".tar.gz" }
}

/// Úprava velikosti písmen při hromadném přejmenování (Ctrl+M).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameCase {
    None,
    Upper,
    Lower,
    Capitalize,
}

/// Capitalizuje první písmeno každého "slova" (za mezerou/podtržítkem/
/// pomlčkou/tečkou), zbytek převede na malá písmena.
fn capitalize_words(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut cap_next = true;
    for ch in s.chars() {
        if cap_next && ch.is_alphabetic() {
            result.extend(ch.to_uppercase());
            cap_next = false;
        } else {
            result.extend(ch.to_lowercase());
            cap_next = !ch.is_alphanumeric();
        }
    }
    result
}

struct FileManagerApp {
    left: Panel,
    right: Panel,
    active: ActivePanel,

    op_rx: Option<Receiver<OpMsg>>,
    op_flag: Option<OpFlag>,
    err_flag: Arc<AtomicU8>,
    op_err_wait: Option<(String, String)>, // (soubor, chyba)
    op_err_focus: usize, // 0=Opakovat, 1=Přeskočit, 2=Zrušit
    op_status: Option<StatusMsg>,
    op_kind_label: String,
    op_done: usize,
    op_total: usize,
    op_bytes_done: u64,
    op_bytes_total: u64,
    op_skipped: usize,
    op_errors: Vec<String>,
    op_slots: Vec<Option<FileSlot>>,  // per-thread aktivní soubory
    show_progress: bool,

    show_delete_confirm: bool,
    delete_targets: Vec<PathBuf>,
    delete_focus: usize,   // 0=Smazat, 1=Zrušit

    show_copy_confirm: bool,
    copy_move_kind: OpKind,
    copy_targets: Vec<PathBuf>,
    copy_dst: PathBuf,
    copy_focus: usize,
    copy_new_name: String,

    show_zip_confirm: bool,
    zip_pack_files: Vec<PathBuf>,
    zip_pack_dst: PathBuf,
    zip_pack_name: String,
    zip_focus: usize,  // přejmenování při kopírování/přesunu jedné položky
    overwrite_focus: usize, // 0=Přepsat, 1=Přeskočit, 2=Zrušit

    show_rename_dialog: bool,
    rename_pattern: String,
    rename_replacement: String,
    rename_use_regex: bool,
    rename_error: Option<String>,
    // Maska + počítadlo + velikost písmen (rozšíření hromadného přejmenování)
    rename_mask: String,
    rename_counter_start: i64,
    rename_counter_step: i64,
    rename_counter_width: usize,
    rename_case: RenameCase,

    // F2 - přejmenování jednoho souboru (inline, ne hromadné)
    show_rename_single: bool,
    rename_single_old: String,
    rename_single_new: String,
    rename_single_error: Option<String>,

    show_select_mask: bool,
    select_mask: String,
    select_mask_add: bool,

    show_new_dir: bool,
    new_dir_name: String,
    new_dir_error: Option<String>,

    show_new_file: bool,
    new_file_name: String,
    new_file_error: Option<String>,

    // Drag & drop - přesun souborů tažením myší
    drag_src: Option<ActivePanel>,

    // Textový editor
    show_text_editor: bool,
    text_editor_path: PathBuf,
    text_editor_content: String,
    text_editor_modified: bool,
    text_editor_jump_start: bool,

    // Barevné téma
    dark_mode: bool,
    show_hidden: bool,
    external_editor: String,  // cesta k externímu editoru (exe)
    dir_sort: DirSort,

    // Auto-update
    update_check_rx: Option<std::sync::mpsc::Receiver<Result<UpdateCheckResult, String>>>,
    update_state: UpdateState,
    show_update_dialog: bool,
    maximize_on_start: bool,
    icon_set_on_start: bool,
    update_auto_checked: bool, // automatická kontrola proběhla při startu

    pending_action: Option<PendingAction>,
    pending_conflicts: Vec<String>,

    show_search: bool,
    search_root: PathBuf,
    search_name: String,
    search_content: String,
    search_rx: Option<Receiver<SearchMsg>>,
    search_results: Vec<PathBuf>,
    search_scanned: usize,
    search_running: bool,

    // Kontrolní součet (MD5 / SHA-256)
    show_hash_dialog: bool,
    hash_rx: Option<Receiver<HashMsg>>,
    hash_flag: Option<OpFlag>,
    hash_results: Vec<(String, String, String)>, // (název, MD5, SHA-256)
    hash_errors: Vec<String>,
    hash_running: bool,
    hash_done: usize,
    hash_total: usize,
    hash_current: String,

    show_about: bool,
    show_settings: bool,

    bookmarks: Vec<Bookmark>,
    show_bookmarks: bool,
    bookmarks_target: ActivePanel,
    // Inline editace záložky
    bm_edit_idx:   Option<usize>, // která záložka se edituje
    bm_edit_name:  String,
    bm_edit_left:  String,
    bm_edit_right: String,

    // Cache poslední uložené hodnoty, ať appka nezapisuje stav na disk
    // při každém překreslení, ale jen když se skutečně něco změnilo.
    last_saved_left: PathBuf,
    last_saved_right: PathBuf,

    // Sledování změn v souborovém systému (auto-refresh)
    fs_watcher: Option<RecommendedWatcher>,
    fs_rx: Option<std::sync::mpsc::Receiver<Result<FsEvent, notify::Error>>>,
    watched_left: PathBuf,
    watched_right: PathBuf,

    // Výsledky výpočtu velikostí složek (path, size)
    dir_size_rx: Option<Receiver<(PathBuf, u64)>>,
}

impl Default for FileManagerApp {
    fn default() -> Self {
        let home = dirs_home();
        let (saved_left, saved_right, bookmarks,
             sort_l_col, sort_l_dir, sort_r_col, sort_r_dir, saved_dark_mode) = load_state();
        let left_path  = saved_left.unwrap_or_else(|| home.clone());
        let right_path = saved_right.unwrap_or_else(|| home.clone());
        let mut left_panel  = Panel::new(left_path.clone());
        let mut right_panel = Panel::new(right_path.clone());
        if let Some(c) = sort_l_col { left_panel.sort_col  = c; left_panel.refresh(); }
        if let Some(d) = sort_l_dir { left_panel.sort_dir  = d; left_panel.refresh(); }
        if let Some(c) = sort_r_col { right_panel.sort_col = c; right_panel.refresh(); }
        if let Some(d) = sort_r_dir { right_panel.sort_dir = d; right_panel.refresh(); }

        // dir_sort načteme a aplikujeme na oba panely
        let saved_dir_sort = fs::read_to_string(state_file_path()).ok()
            .and_then(|c| c.lines()
                .find(|l| l.starts_with("DIR_SORT="))
                .map(|l| match l["DIR_SORT=".len()..].trim() {
                    "FirstByName" => DirSort::FirstByName,
                    "Mixed"       => DirSort::Mixed,
                    _             => DirSort::FirstByCol,
                }))
            .unwrap_or(DirSort::FirstByCol);
        left_panel.dir_sort  = saved_dir_sort;
        right_panel.dir_sort = saved_dir_sort;
        if saved_dir_sort != DirSort::FirstByCol {
            left_panel.refresh();
            right_panel.refresh();
        }

        let saved_show_hidden = std::fs::read_to_string(state_file_path()).ok()
            .and_then(|c| c.lines()
                .find(|l| l.starts_with("SHOW_HIDDEN="))
                .map(|l| l["SHOW_HIDDEN=".len()..].trim() == "true"))
            .unwrap_or(false);
        if saved_show_hidden {
            left_panel.show_hidden  = true;
            right_panel.show_hidden = true;
            left_panel.refresh();
            right_panel.refresh();
        }

        Self {
            left: left_panel,
            right: right_panel,
            active: ActivePanel::Left,
            op_rx: None,
            op_flag: None,
            err_flag: Arc::new(AtomicU8::new(ERR_WAIT)),
            op_err_wait: None,
            op_err_focus: 0,
            op_status: None,
            op_kind_label: String::new(),
            op_done: 0,
            op_total: 0,
            op_bytes_done: 0,
            op_bytes_total: 0,
            op_skipped: 0,
            op_errors: Vec::new(),
            op_slots: Vec::new(),
            show_progress: false,
            show_delete_confirm: false,
            delete_targets: Vec::new(),
            delete_focus: 0,
            show_copy_confirm: false,
            copy_move_kind: OpKind::Copy,
            copy_targets: Vec::new(),
            copy_dst: PathBuf::new(),
            copy_focus: 0,
            copy_new_name: String::new(),
            show_zip_confirm: false,
            zip_pack_files: Vec::new(),
            zip_pack_dst: PathBuf::new(),
            zip_pack_name: String::new(),
            zip_focus: 0,
            overwrite_focus: 0,
            show_rename_dialog: false,
            rename_pattern: String::new(),
            rename_replacement: String::new(),
            rename_use_regex: false,
            rename_error: None,
            rename_mask: String::new(),
            rename_counter_start: 1,
            rename_counter_step: 1,
            rename_counter_width: 2,
            rename_case: RenameCase::None,
            show_rename_single: false,
            rename_single_old: String::new(),
            rename_single_new: String::new(),
            rename_single_error: None,
            show_select_mask: false,
            select_mask: "*.".to_string(),
            select_mask_add: true,
            show_new_dir: false,
            new_dir_name: String::new(),
            new_dir_error: None,
            show_new_file: false,
            new_file_name: String::new(),
            new_file_error: None,
            drag_src: None,
            show_text_editor: false,
            text_editor_path: PathBuf::new(),
            text_editor_content: String::new(),
            text_editor_modified: false,
            text_editor_jump_start: false,
            dark_mode: saved_dark_mode,
            show_hidden: std::fs::read_to_string(state_file_path()).ok()
                .and_then(|c| c.lines()
                    .find(|l| l.starts_with("SHOW_HIDDEN="))
                    .map(|l| l["SHOW_HIDDEN=".len()..].trim() == "true"))
                .unwrap_or(false),
            external_editor: fs::read_to_string(state_file_path()).ok()
                .and_then(|c| c.lines()
                    .find(|l| l.starts_with("EXT_EDITOR="))
                    .map(|l| l["EXT_EDITOR=".len()..].trim().to_string()))
                .unwrap_or_default(),
            dir_sort: fs::read_to_string(state_file_path()).ok()
                .and_then(|c| c.lines()
                    .find(|l| l.starts_with("DIR_SORT="))
                    .map(|l| match l["DIR_SORT=".len()..].trim() {
                        "FirstByName" => DirSort::FirstByName,
                        "Mixed"       => DirSort::Mixed,
                        _             => DirSort::FirstByCol,
                    }))
                .unwrap_or(DirSort::FirstByCol),
            update_check_rx: None,
            update_state: UpdateState::Idle,
            show_update_dialog: false,
            maximize_on_start: false,
            icon_set_on_start: false,
            update_auto_checked: false,
            pending_action: None,
            pending_conflicts: Vec::new(),
            show_search: false,
            search_root: home,
            search_name: String::new(),
            search_content: String::new(),
            search_rx: None,
            search_results: Vec::new(),
            search_scanned: 0,
            search_running: false,
            show_hash_dialog: false,
            hash_rx: None,
            hash_flag: None,
            hash_results: Vec::new(),
            hash_errors: Vec::new(),
            hash_running: false,
            hash_done: 0,
            hash_total: 0,
            hash_current: String::new(),
            show_about: false,
            show_settings: false,
            bookmarks,
            show_bookmarks: false,
            bookmarks_target: ActivePanel::Left,
            bm_edit_idx:   None,
            bm_edit_name:  String::new(),
            bm_edit_left:  String::new(),
            bm_edit_right: String::new(),
            last_saved_left: left_path,
            last_saved_right: right_path,
            fs_watcher: None,
            fs_rx: None,
            watched_left: PathBuf::new(),
            watched_right: PathBuf::new(),
            dir_size_rx: None,
        }
    }
}

impl FileManagerApp {
    fn active_panel(&self) -> &Panel {
        match self.active {
            ActivePanel::Left => &self.left,
            ActivePanel::Right => &self.right,
        }
    }

    fn active_panel_mut(&mut self) -> &mut Panel {
        match self.active {
            ActivePanel::Left => &mut self.left,
            ActivePanel::Right => &mut self.right,
        }
    }

    fn inactive_dir(&self) -> PathBuf {
        match self.active {
            ActivePanel::Left => self.right.current_path.clone(),
            ActivePanel::Right => self.left.current_path.clone(),
        }
    }

    fn refresh_both(&mut self) {
        self.left.refresh();
        self.right.refresh();
    }

    /// Centralizovaný start operace - nastaví všechna progress pole a otevře dialog.
    fn start_op(&mut self, label: &str, rx: Receiver<OpMsg>, flag: OpFlag, total: usize) {
        let threads = 4.min(total).max(1);
        self.op_rx          = Some(rx);
        self.op_flag        = Some(flag);
        self.op_kind_label  = label.to_string();
        self.op_done        = 0;
        self.op_total       = total;
        self.op_bytes_done  = 0;
        self.op_bytes_total = 0;
        self.op_skipped     = 0;
        self.op_errors      = Vec::new();
        self.op_slots       = vec![None; threads];
        self.op_status = Some(StatusMsg::Info(format!("{}...", label)));
        self.show_progress  = true;
        self.op_err_wait    = None;
        self.err_flag.store(ERR_WAIT, Ordering::Relaxed);
    }

    fn start_copy(&mut self) {
        let panel = self.active_panel();

        // ZIP archiv - bez confirm dialogu, rovnou na přesun/konflikt
        if let Some(loc) = panel.archive_location.clone() {
            let names = panel.effective_names();
            if names.is_empty() { return; }
            let dst = self.inactive_dir();
            let conflicts = names_conflicting(&names, &dst);
            if conflicts.is_empty() {
                let n = names.len();
                let (rx, flag) = spawn_zip_extract(loc.archive_path, loc.internal_dir, names, dst, true, self.err_flag.clone());
                self.start_op("Rozbaluji ZIP", rx, flag, n);
            } else {
                self.pending_conflicts = conflicts;
                self.overwrite_focus = 0;
                self.pending_action = Some(PendingAction::ZipExtract {
                    archive_path: loc.archive_path, internal_dir: loc.internal_dir, names, dst,
                });
            }
            return;
        }

        let files = panel.effective_paths();
        if files.is_empty() { return; }
        let dst = self.inactive_dir();
        // Při jednom souboru předvyplníme jméno pro případné přejmenování
        self.copy_new_name = if files.len() == 1 {
            files[0].file_name().unwrap_or_default().to_string_lossy().into_owned()
        } else { String::new() };
        self.copy_targets  = files;
        self.copy_dst      = dst;
        self.copy_move_kind = OpKind::Copy;
        self.copy_focus = 0;
        self.show_copy_confirm = true;
    }

    fn start_move(&mut self) {
        if self.active_panel().archive_location.is_some() {
            self.op_status = Some(StatusMsg::Error("Přesun uvnitř archivu není podporovaný.".to_string()));
            return;
        }
        let files = self.active_panel().effective_paths();
        if files.is_empty() { return; }
        let dst = self.inactive_dir();
        self.copy_new_name = if files.len() == 1 {
            files[0].file_name().unwrap_or_default().to_string_lossy().into_owned()
        } else { String::new() };
        self.copy_targets   = files;
        self.copy_dst       = dst;
        self.copy_move_kind = OpKind::Move;
        self.copy_focus = 0;
        self.show_copy_confirm = true;
    }

    fn confirm_copy_move(&mut self) {
        let files    = std::mem::take(&mut self.copy_targets);
        let dst      = std::mem::take(&mut self.copy_dst);
        let kind     = self.copy_move_kind;
        let new_name = self.copy_new_name.trim().to_string();
        self.show_copy_confirm = false;

        // dst_name: přejmenování při kopírování jednoho souboru
        let dst_name = if files.len() == 1 && !new_name.is_empty() {
            let orig = files[0].file_name()
                .unwrap_or_default().to_string_lossy().to_string();
            if new_name != orig { Some(new_name) } else { None }
        } else { None };

        let conflicts = paths_conflicting(&files, &dst);
        if conflicts.is_empty() {
            let label = if kind == OpKind::Copy { "Kopírování" } else { "Přesun" };
            let n = files.len();
            let (rx, flag) = spawn_file_op(kind, files, Some(dst), true, self.err_flag.clone(), dst_name);
            self.start_op(label, rx, flag, n);
        } else {
            self.pending_conflicts = conflicts;
            self.pending_action = Some(PendingAction::FsOp { kind, files, dst });
            self.overwrite_focus = 0;
        }
    }

    fn resolve_pending(&mut self, overwrite: bool) {
        let Some(action) = self.pending_action.take() else { return; };
        self.pending_conflicts.clear();
        match action {
            PendingAction::FsOp { kind, files, dst } => {
                let label = match kind {
                    OpKind::Copy   => "Kopírování",
                    OpKind::Move   => "Přesun",
                    OpKind::Delete => "Mazání",
                };
                let n = files.len();
                let (rx, flag) = spawn_file_op(kind, files, Some(dst), overwrite, self.err_flag.clone(), None);
                self.start_op(label, rx, flag, n);
            }
            PendingAction::ZipExtract { archive_path, internal_dir, names, dst } => {
                let n = names.len();
                let (rx, flag) = spawn_zip_extract(archive_path, internal_dir, names, dst, overwrite, self.err_flag.clone());
                self.start_op("Rozbaluji ZIP", rx, flag, n);
            }
        }
    }

    fn cancel_pending(&mut self) {
        self.pending_action = None;
        self.pending_conflicts.clear();
    }

    /// Otevře dialog hledání (Alt+F7), s výchozím kořenem = aktuální
    /// adresář aktivního panelu.
    fn open_search_dialog(&mut self) {
        self.search_root = self.active_panel().current_path.clone();
        self.search_name.clear();
        self.search_content.clear();
        self.search_results.clear();
        self.search_running = false;
        self.show_search = true;
    }

    fn start_search(&mut self) {
        if !self.search_root.is_dir() {
            self.op_status = Some(StatusMsg::Error("Zadaný adresář pro hledání neexistuje.".to_string()));
            return;
        }
        self.search_results.clear();
        self.search_scanned = 0;
        self.search_running = true;
        self.search_rx = Some(spawn_search(
            self.search_root.clone(),
            self.search_name.clone(),
            self.search_content.clone(),
        ));
    }

    fn poll_search(&mut self) {
        let Some(rx) = self.search_rx.take() else {
            return;
        };

        let mut still_running = true;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                SearchMsg::Found(path) => self.search_results.push(path),
                SearchMsg::Scanned(n) => self.search_scanned = n,
                SearchMsg::Finished => still_running = false,
            }
        }

        if still_running {
            self.search_rx = Some(rx);
        } else {
            self.search_running = false;
        }
    }

    /// Spočítá MD5/SHA-256 pro vybrané soubory (nebo soubor pod kurzorem,
    /// pokud nic není vybráno). Adresáře se přeskakují.
    fn open_hash_dialog(&mut self) {
        let files: Vec<PathBuf> = self.active_panel().effective_paths()
            .into_iter()
            .filter(|p| p.is_file())
            .collect();

        if files.is_empty() {
            self.op_status = Some(StatusMsg::Warn(
                "Vyber alespoň jeden soubor pro výpočet kontrolního součtu.".to_string()));
            return;
        }

        self.hash_results.clear();
        self.hash_errors.clear();
        self.hash_done = 0;
        self.hash_total = files.len();
        self.hash_current.clear();
        self.hash_running = true;
        self.show_hash_dialog = true;

        let flag: OpFlag = Arc::new(AtomicU8::new(OP_RUN));
        self.hash_flag = Some(flag.clone());
        self.hash_rx = Some(spawn_hash_calc(files, flag));
    }

    fn poll_hash(&mut self) {
        let Some(rx) = self.hash_rx.take() else { return };

        let mut still_running = true;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                HashMsg::Progress { done, total, current } => {
                    self.hash_done = done;
                    self.hash_total = total;
                    self.hash_current = current;
                }
                HashMsg::Result { name, md5, sha256 } => {
                    self.hash_results.push((name, md5, sha256));
                }
                HashMsg::Error { name, msg } => {
                    self.hash_errors.push(format!("{}: {}", name, msg));
                }
                HashMsg::Finished => still_running = false,
            }
        }

        if still_running {
            self.hash_rx = Some(rx);
        } else {
            self.hash_running = false;
        }
    }

    /// Přeskočí v panelu na nalezený soubor (otevře jeho složku a soubor
    /// rovnou označí), ať se k němu uživatel může rovnou dostat.
    fn jump_to_search_result(&mut self, path: &Path) {
        let Some(parent) = path.parent() else { return };
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned());

        let panel = self.active_panel_mut();
        panel.archive_location = None;
        panel.current_path = parent.to_path_buf();
        panel.refresh();

        if let Some(name) = file_name {
            if let Some(idx) = panel.entries.iter().position(|e| e.name == name) {
                panel.cursor = idx + panel.up_offset();
                panel.selected = vec![idx];
            }
        }
    }

    /// Nastaví file system watcher na aktuální složky obou panelů.
    /// Volá se kdykoli se změní sledovaná cesta.
    fn update_watcher(&mut self) {
        let left  = self.left.current_path.clone();
        let right = self.right.current_path.clone();

        // Pokud se cesty nezměnily, nic neděláme
        if left == self.watched_left && right == self.watched_right {
            return;
        }

        // Vytvoříme nový watcher s channel pro události
        let (tx, rx) = std::sync::mpsc::channel();
        match notify::recommended_watcher(tx) {
            Ok(mut watcher) => {
                // Sledujeme obě složky (non-recursive - jen přímý obsah)
                let _ = watcher.watch(&left,  RecursiveMode::NonRecursive);
                if right != left {
                    let _ = watcher.watch(&right, RecursiveMode::NonRecursive);
                }
                self.fs_watcher   = Some(watcher);
                self.fs_rx        = Some(rx);
                self.watched_left  = left;
                self.watched_right = right;
            }
            Err(_) => {
                // Notify nemusí fungovat na všech FS (síťové disky apod.)
                // - tiše ignorujeme
                self.fs_watcher = None;
                self.fs_rx      = None;
            }
        }
    }

    /// Zpracuje události z file system watcheru - při změně refreshne panel.
    fn poll_fs_events(&mut self) {
        let Some(rx) = &self.fs_rx else { return };
        let mut refresh_left  = false;
        let mut refresh_right = false;

        while let Ok(Ok(event)) = rx.try_recv() {
            // Zajímají nás jen události které mění obsah adresáře
            use notify::EventKind::*;
            match event.kind {
                Create(_) | Remove(_) | Modify(notify::event::ModifyKind::Name(_)) => {
                    for path in &event.paths {
                        if let Some(parent) = path.parent() {
                            if parent == self.left.current_path {
                                refresh_left = true;
                            }
                            if parent == self.right.current_path {
                                refresh_right = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if refresh_left  { self.left.refresh(); }
        if refresh_right { self.right.refresh(); }
    }

    /// Uloží aktuální stav (naposledy otevřené složky + záložky) na disk,
    /// ať appka po dalším spuštění naváže tam, kde se skončilo.
    /// Spustí výpočet velikostí vybraných složek na pozadí.
    /// Volá se při označení složky (mezerník/klik).
    fn start_dir_size_calc(&mut self) {
        // Sesbíráme cesty ke všem vybraným složkám z obou panelů
        let mut dirs: Vec<PathBuf> = Vec::new();
        for panel in [&self.left, &self.right] {
            for &i in &panel.selected {
                if let Some(e) = panel.entries.get(i) {
                    if e.is_dir && e.dir_size.is_none() {
                        dirs.push(panel.current_path.join(&e.name));
                    }
                }
            }
        }
        if dirs.is_empty() { return; }

        let (tx, rx) = channel();
        thread::spawn(move || {
            for dir in dirs {
                let size: u64 = WalkDir::new(&dir)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_file())
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum();
                let _ = tx.send((dir, size));
            }
        });
        self.dir_size_rx = Some(rx);
    }

    /// Zpracuje výsledky výpočtu velikostí - zapíše je do příslušných FileEntry.
    fn poll_dir_sizes(&mut self) {
        let Some(rx) = self.dir_size_rx.take() else { return };
        let mut got_data = false;

        while let Ok((path, size)) = rx.try_recv() {
            got_data = true;
            for panel in [&mut self.left, &mut self.right] {
                for entry in panel.entries.iter_mut() {
                    if entry.is_dir && panel.current_path.join(&entry.name) == path {
                        entry.dir_size = Some(size);
                    }
                }
            }
        }
        let _ = got_data;

        // Zkusíme jestli channel stále žije (Disconnected = thread skončil)
        match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Thread skončil, zahodíme rx
            }
            _ => {
                // Thread stále běží nebo má data, vrátíme rx
                self.dir_size_rx = Some(rx);
            }
        }
    }

    fn save_state(&self) {
        let sort_col_str = |c: SortColumn| match c {
            SortColumn::Name => "Name", SortColumn::Ext  => "Ext",
            SortColumn::Size => "Size", SortColumn::Date => "Date",
            SortColumn::Attr => "Attr",
        };
        let sort_dir_str = |d: SortDir| if d == SortDir::Asc { "Asc" } else { "Desc" };

        let mut content = String::new();
        content.push_str(&format!("LEFT={}\n",        self.left.current_path.display()));
        content.push_str(&format!("RIGHT={}\n",       self.right.current_path.display()));
        content.push_str(&format!("SORT_L_COL={}\n",  sort_col_str(self.left.sort_col)));
        content.push_str(&format!("SORT_L_DIR={}\n",  sort_dir_str(self.left.sort_dir)));
        content.push_str(&format!("SORT_R_COL={}\n",  sort_col_str(self.right.sort_col)));
        content.push_str(&format!("SORT_R_DIR={}\n",  sort_dir_str(self.right.sort_dir)));
        content.push_str(&format!("DARK_MODE={}\n",   self.dark_mode));
        content.push_str(&format!("SHOW_HIDDEN={}\n", self.show_hidden));
        content.push_str(&format!("EXT_EDITOR={}\n",  self.external_editor));
        content.push_str(&format!("DIR_SORT={}\n", match self.dir_sort {
            DirSort::FirstByName => "FirstByName",
            DirSort::FirstByCol  => "FirstByCol",
            DirSort::Mixed       => "Mixed",
        }));
        for bm in &self.bookmarks {
            content.push_str(&format!("BM_NAME={}\n", bm.name));
            content.push_str(&format!("BM_LEFT={}\n", bm.left.display()));
            content.push_str(&format!("BM_RIGHT={}\n",
                bm.right.as_ref().map(|p| p.display().to_string()).unwrap_or_default()));
        }
        let _ = fs::write(state_file_path(), content);
    }

    fn save_window_state(&self, ctx: &egui::Context) {
        let info = ctx.input(|i| i.viewport().clone());
        let maximized  = info.maximized.unwrap_or(false);
        let minimized  = info.minimized.unwrap_or(false);

        let mut content = String::new();
        content.push_str(&format!("WIN_MAX={}\n", maximized));
        content.push_str(&format!("WIN_MIN={}\n", minimized));

        // outer_rect ukládáme jen pokud není maximalizované/minimalizované
        // - jinak bychom uložili celou obrazovku a při obnovení by se neotevřelo maximalizovaně
        if !maximized && !minimized {
            if let Some(pos) = info.outer_rect {
                content.push_str(&format!("WIN_X={}\n", pos.min.x as i32));
                content.push_str(&format!("WIN_Y={}\n", pos.min.y as i32));
                content.push_str(&format!("WIN_W={}\n", pos.width() as i32));
                content.push_str(&format!("WIN_H={}\n", pos.height() as i32));
            }
        }

        let path = dirs_home().join(".er_commander_window.txt");
        let _ = fs::write(path, content);
    }

    /// Otevře dialog záložek. `target` říká, do kterého panelu se má
    /// vybraná záložka skočit (ten, ze kterého se dialog otevřel hvězdičkou).
    fn open_bookmarks(&mut self, target: ActivePanel) {
        self.bookmarks_target = target;
        self.show_bookmarks = true;
    }

    fn add_bookmark_current(&mut self) {
        // Uloží oba panely – aktivní jako "levý" (první), neaktivní jako "pravý" (druhý)
        let (active_path, inactive_path) = match self.bookmarks_target {
            ActivePanel::Left  => (self.left.current_path.clone(),  self.right.current_path.clone()),
            ActivePanel::Right => (self.right.current_path.clone(), self.left.current_path.clone()),
        };
        let name = active_path.file_name()
            .unwrap_or_default().to_string_lossy().into_owned();
        self.bookmarks.push(Bookmark {
            name,
            left:  active_path,
            right: Some(inactive_path),
        });
        self.save_state();
    }

    fn add_bookmark_single(&mut self) {
        // Uloží jen aktivní panel (ten ze kterého se kliklo na hvězdičku)
        let path = match self.bookmarks_target {
            ActivePanel::Left  => self.left.current_path.clone(),
            ActivePanel::Right => self.right.current_path.clone(),
        };
        let name = path.file_name()
            .unwrap_or_default().to_string_lossy().into_owned();
        self.bookmarks.push(Bookmark { name, left: path, right: None });
        self.save_state();
    }

    fn remove_bookmark(&mut self, idx: usize) {
        if idx < self.bookmarks.len() {
            self.bookmarks.remove(idx);
            self.save_state();
        }
    }

    fn jump_to_bookmark(&mut self, bm: &Bookmark) {
        // bm.left = aktivní panel (ze kterého se kliklo na hvězdičku)
        // bm.right = neaktivní panel
        match self.bookmarks_target {
            ActivePanel::Left => {
                // Kliklo se na hvězdičku v levém panelu
                self.left.archive_location = None;
                self.left.current_path = bm.left.clone();
                self.left.refresh();
                if let Some(ref inactive) = bm.right {
                    self.right.archive_location = None;
                    self.right.current_path = inactive.clone();
                    self.right.refresh();
                }
                self.active = ActivePanel::Left;
            }
            ActivePanel::Right => {
                // Kliklo se na hvězdičku v pravém panelu
                self.right.archive_location = None;
                self.right.current_path = bm.left.clone();
                self.right.refresh();
                if let Some(ref inactive) = bm.right {
                    self.left.archive_location = None;
                    self.left.current_path = inactive.clone();
                    self.left.refresh();
                }
                self.active = ActivePanel::Right;
            }
        }
    }

    fn request_delete(&mut self) {
        if self.active_panel().archive_location.is_some() {
            self.op_status = Some(StatusMsg::Error("Mazání uvnitř archivu není podporované.".to_string()));
            return;
        }
        let files = self.active_panel().effective_paths();
        if files.is_empty() {
            return;
        }
        self.delete_targets = files;
        self.delete_focus = 0;
        self.show_delete_confirm = true;
    }

    fn confirm_delete(&mut self) {
        let files = std::mem::take(&mut self.delete_targets);
        let n = files.len();
        let (rx, flag) = spawn_file_op(OpKind::Delete, files, None, true, self.err_flag.clone(), None);
        self.start_op("Mazání", rx, flag, n);
        self.show_delete_confirm = false;
    }

    fn poll_op_progress(&mut self) {
        let Some(rx) = self.op_rx.take() else { return; };
        let mut still_running = true;

        while let Ok(msg) = rx.try_recv() {
            match msg {
                OpMsg::SlotStart { thread_id, file, file_size } => {
                    if let Some(slot) = self.op_slots.get_mut(thread_id) {
                        *slot = Some(FileSlot { file, size: file_size, copied: 0 });
                    }
                }
                OpMsg::SlotProgress { thread_id, bytes_copied } => {
                    if let Some(Some(slot)) = self.op_slots.get_mut(thread_id) {
                        slot.copied += bytes_copied;
                    }
                }
                OpMsg::SlotDone { thread_id } => {
                    if let Some(slot) = self.op_slots.get_mut(thread_id) {
                        *slot = None;
                    }
                }
                OpMsg::Progress { done, total, bytes_done, bytes_total } => {
                    self.op_done        = done;
                    self.op_total       = total;
                    self.op_bytes_done  = bytes_done;
                    self.op_bytes_total = bytes_total;
                }
                OpMsg::Skipped(_) => {
                    self.op_skipped += 1;
                    self.op_done = (self.op_done + 1).min(self.op_total);
                }
                OpMsg::Finished => {
                    self.op_done   = self.op_total;
                    self.op_status = Some(StatusMsg::Info("Hotovo.".to_string()));
                    still_running  = false;
                    self.op_slots  = Vec::new();
                    self.refresh_both();
                }
                OpMsg::Error(e) => {
                    self.op_errors.push(e.clone());
                    self.op_status = Some(StatusMsg::Error(format!("Chyba: {}", e)));
                    still_running  = false;
                    self.op_slots  = Vec::new();
                    self.refresh_both();
                }
                OpMsg::ErrorWait { file, error } => {
                    self.op_err_wait  = Some((file, error));
                    self.op_err_focus = 0;
                    self.err_flag.store(ERR_WAIT, Ordering::Relaxed);
                    self.op_rx = Some(rx);
                    return;
                }
            }
        }

        if still_running {
            self.op_rx = Some(rx);
        } else {
            self.op_flag = None;
        }
    }

    fn resolve_op_error(&mut self) {
        let answer = match self.op_err_focus {
            0 => ERR_RETRY,
            1 => ERR_SKIP,
            _ => ERR_CANCEL,
        };
        self.err_flag.store(answer, Ordering::Relaxed);
        self.op_err_wait = None;
        if answer == ERR_CANCEL {
            if let Some(f) = &self.op_flag { f.store(OP_STOP, Ordering::Relaxed); }
        }
    }

    fn render_progress_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_progress { return; }

        // Chybový dialog - zobrazí se místo progress barů
        if let Some((file, error)) = self.op_err_wait.clone() {
            let mut retry = false; let mut skip = false; let mut cancel = false;
            egui::Window::new("⚠ Chyba při operaci")
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .min_width(420.0)
                .show(ctx, |ui| {
                    ui.colored_label(egui::Color32::from_rgb(255,100,100),
                        egui::RichText::new(&error).strong());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| { ui.label("Soubor:"); ui.label(egui::RichText::new(&file).monospace()); });
                    ui.separator();
                    ui.label(egui::RichText::new("← → šipky volí  |  Enter potvrdí  |  Esc = Zrušit")
                        .small().color(egui::Color32::GRAY));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let btns = [("🔄 Opakovat", 0usize), ("⏭ Přeskočit", 1), ("🚫 Zrušit vše", 2)];
                        for (label, idx) in btns {
                            let r = ui.button(label);
                            if self.op_err_focus == idx {
                                ui.painter().rect_stroke(r.rect, 3.0,
                                    egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255,190,60)));
                            }
                            match (r.clicked(), idx) {
                                (true, 0) => retry  = true,
                                (true, 1) => skip   = true,
                                (true, 2) => cancel = true,
                                _ => {}
                            }
                        }
                    });
                });
            if retry  { self.op_err_focus = 0; self.resolve_op_error(); }
            if skip   { self.op_err_focus = 1; self.resolve_op_error(); }
            if cancel { self.op_err_focus = 2; self.resolve_op_error(); }
            return;
        }

        let is_running = self.op_rx.is_some();
        let is_paused  = self.op_flag.as_ref()
            .map(|f| f.load(Ordering::Relaxed) == OP_PAUSE)
            .unwrap_or(false);

        // Snapshot slotů pro render (ať nemusíme borrowit self uvnitř closure)
        let slots_snap: Vec<Option<FileSlot>> = self.op_slots.clone();

        egui::Window::new(format!("{} – průběh", self.op_kind_label))
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .min_width(460.0)
            .show(ctx, |ui| {
                // ── Per-soubor slots ──────────────────────────────────────
                let active_slots: Vec<&FileSlot> = slots_snap.iter()
                    .filter_map(|s| s.as_ref()).collect();

                if !active_slots.is_empty() {
                    ui.label(egui::RichText::new("Právě přenášeno:").strong());
                    for slot in &active_slots {
                        let frac = if slot.size > 0 {
                            slot.copied as f32 / slot.size as f32
                        } else { 1.0 };
                        ui.horizontal(|ui| {
                            // Zkrácené jméno souboru - unicode-safe přes chars()
                            let chars: Vec<char> = slot.file.chars().collect();
                            let name = if chars.len() > 34 {
                                format!("...{}", &chars[chars.len()-31..].iter().collect::<String>())
                            } else {
                                slot.file.clone()
                            };
                            ui.label(egui::RichText::new(&name).monospace().small());
                        });
                        ui.add(egui::ProgressBar::new(frac)
                            .text(if slot.size > 0 {
                                format!("{} / {}", format_size(slot.copied), format_size(slot.size))
                            } else {
                                "přenáším...".to_string()
                            })
                            .desired_width(440.0));
                    }
                    ui.separator();
                }

                // ── Celkový progress ──────────────────────────────────────
                ui.label(egui::RichText::new("Celkem:").strong());

                let file_frac = if self.op_total > 0 {
                    self.op_done as f32 / self.op_total as f32
                } else { 0.0 };
                ui.add(egui::ProgressBar::new(file_frac)
                    .text(format!("{} / {} souborů", self.op_done, self.op_total))
                    .desired_width(440.0));

                if self.op_bytes_total > 0 {
                    let byte_frac = (self.op_bytes_done as f32 / self.op_bytes_total as f32).min(1.0);
                    ui.add(egui::ProgressBar::new(byte_frac)
                        .text(format!("{} / {}", format_size(self.op_bytes_done),
                                                  format_size(self.op_bytes_total)))
                        .desired_width(440.0));
                }

                // ── Statistiky ────────────────────────────────────────────
                if self.op_skipped > 0 || !self.op_errors.is_empty() {
                    ui.add_space(4.0);
                    if self.op_skipped > 0 {
                        ui.colored_label(egui::Color32::from_rgb(200, 180, 50),
                            format!("Přeskočeno: {} souboru", self.op_skipped));
                    }
                    if !self.op_errors.is_empty() {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80),
                            format!("Chyby: {}", self.op_errors.len()));
                        egui::ScrollArea::vertical().max_height(80.0).show(ui, |ui| {
                            for e in &self.op_errors {
                                ui.label(egui::RichText::new(e)
                                    .color(egui::Color32::from_rgb(220,80,80)).small());
                            }
                        });
                    }
                }

                if !is_running {
                    ui.add_space(4.0);
                    if let Some(s) = &self.op_status {
                        ui.colored_label(s.color(), egui::RichText::new(s.text()).strong());
                    } else {
                        ui.label(egui::RichText::new("Hotovo.").strong());
                    }
                }

                // ── Tlačítka ──────────────────────────────────────────────
                ui.separator();
                ui.horizontal(|ui| {
                    if is_running {
                        if is_paused {
                            if ui.button("Pokracovat").clicked() {
                                if let Some(f) = &self.op_flag {
                                    f.store(OP_RUN, Ordering::Relaxed);
                                }
                            }
                        } else {
                            if ui.button("Pozastavit").clicked() {
                                if let Some(f) = &self.op_flag {
                                    f.store(OP_PAUSE, Ordering::Relaxed);
                                }
                            }
                        }
                        if ui.button("Zastavit").clicked() {
                            if let Some(f) = &self.op_flag {
                                f.store(OP_STOP, Ordering::Relaxed);
                            }
                        }
                    } else {
                        if ui.button("Zavrit").clicked() {
                            self.show_progress = false;
                        }
                    }
                });
            });
    }

    fn rename_preview(&self) -> Result<Vec<(String, String)>, String> {
        let panel = self.active_panel();
        let names: Vec<String> = panel
            .selected
            .iter()
            .filter_map(|&i| panel.entries.get(i))
            .map(|e| e.name.clone())
            .collect();

        // Regex zkompilujeme jen jednou, ne pro každou položku zvlášť.
        let regex = if self.rename_use_regex && !self.rename_pattern.is_empty() {
            Some(Regex::new(&self.rename_pattern).map_err(|e| format!("Neplatný regex: {}", e))?)
        } else {
            None
        };
        let width = self.rename_counter_width.clamp(1, 8);

        let result = names
            .into_iter()
            .enumerate()
            .map(|(idx, old_name)| {
                // 1) Maska ([N]=název bez přípony, [E]=přípona, [C]=počítadlo)
                let mut new_name = if self.rename_mask.trim().is_empty() {
                    old_name.clone()
                } else {
                    let p = Path::new(&old_name);
                    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| old_name.clone());
                    let ext = p.extension().map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let counter_val = self.rename_counter_start + (idx as i64) * self.rename_counter_step;
                    let counter_str = format!("{:0width$}", counter_val, width = width);
                    self.rename_mask
                        .replace("[N]", &stem)
                        .replace("[E]", &ext)
                        .replace("[C]", &counter_str)
                };

                // 2) Najít a nahradit
                if !self.rename_pattern.is_empty() {
                    new_name = if let Some(re) = &regex {
                        re.replace_all(&new_name, self.rename_replacement.as_str()).into_owned()
                    } else {
                        new_name.replace(&self.rename_pattern, &self.rename_replacement)
                    };
                }

                // 3) Velká/malá písmena
                new_name = match self.rename_case {
                    RenameCase::None       => new_name,
                    RenameCase::Upper      => new_name.to_uppercase(),
                    RenameCase::Lower      => new_name.to_lowercase(),
                    RenameCase::Capitalize => capitalize_words(&new_name),
                };

                (old_name, new_name)
            })
            .collect();

        Ok(result)
    }

    fn apply_bulk_rename(&mut self) {
        if self.active_panel().archive_location.is_some() {
            self.rename_error = Some("Přejmenování uvnitř archivu není podporované.".to_string());
            return;
        }

        let preview = match self.rename_preview() {
            Ok(p) => p,
            Err(e) => {
                self.rename_error = Some(e);
                return;
            }
        };

        let current_dir = self.active_panel().current_path.clone();
        for (old_name, new_name) in preview {
            if old_name != new_name {
                let old_path = current_dir.join(&old_name);
                let new_path = current_dir.join(&new_name);
                if let Err(e) = fs::rename(&old_path, &new_path) {
                    self.rename_error = Some(format!("{}: {}", old_name, e));
                }
            }
        }
        self.active_panel_mut().refresh();
    }

    /// F2 - přejmenování jednoho souboru pod kurzorem.
    fn open_rename_single(&mut self) {
        if self.active_panel().archive_location.is_some() { return; }
        let Some(idx) = self.active_panel().entry_index_at_cursor() else { return };
        let Some(name) = self.active_panel().entries.get(idx).map(|e| e.name.clone()) else { return };
        // Aktivujeme inline přejmenování přímo v panelu
        let panel = match self.active {
            ActivePanel::Left  => &mut self.left,
            ActivePanel::Right => &mut self.right,
        };
        panel.inline_rename_idx = Some(idx);
        panel.inline_rename_buf = name;
    }

    fn apply_rename_single(&mut self) {
        let dir = self.active_panel().current_path.clone();
        let old = dir.join(&self.rename_single_old);
        let new_name = self.rename_single_new.clone();
        let new = dir.join(&new_name);

        match fs::rename(&old, &new) {
            Err(e) => {
                self.rename_single_error = Some(e.to_string());
            }
            Ok(()) => {
                self.show_rename_single = false;
                let panel = self.active_panel_mut();
                panel.refresh();
                // Skočíme kurzorem na přejmenovaný soubor.
                if let Some(idx) = panel.entries.iter().position(|e| e.name == new_name) {
                    panel.cursor = idx + panel.up_offset();
                    panel.cursor_moved = true;
                }
            }
        }
    }
}

// =====================================================================
// UI
// =====================================================================

impl eframe::App for FileManagerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Tab spotřebujeme DŘÍVE než cokoliv jiného včetně egui focus navigation.
        // egui zpracovává Tab v InputState.prepare() což nastane při prvním
        // přístupu k input - proto ho musíme consume ještě před tím.
        let editor_id = egui::Id::new("text_editor_area");
        let editor_has_focus = ctx.memory(|m| m.focused() == Some(editor_id));

        // Pokud je editor otevřený ale ztratil focus (Tab ho odnesl jinam),
        // okamžitě ho vrátíme zpátky na editor
        if self.show_text_editor && !editor_has_focus {
            // Spotřebujeme Tab aby neskočil na další widget
            ctx.input_mut(|i| { i.consume_key(egui::Modifiers::NONE, egui::Key::Tab); });
            // A vrátíme focus na editor
            ctx.memory_mut(|m| m.request_focus(editor_id));
        }

        if !self.show_text_editor {
            let tab_pressed = ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
            });
            if tab_pressed {
                self.active = match self.active {
                    ActivePanel::Left  => ActivePanel::Right,
                    ActivePanel::Right => ActivePanel::Left,
                };
                ctx.memory_mut(|m| {
                    m.surrender_focus(egui::Id::new(("path_input", ActivePanel::Left)));
                    m.surrender_focus(egui::Id::new(("path_input", ActivePanel::Right)));
                });
            }
        }

        // Focus cleanup - odstraníme focus z tlačítek a menu
        // (ale ne z textových polí a editoru)
        // Odebíráme focus z widgetů jen pokud nejsou otevřené ŽÁDNÉ dialogy
        // a nejsme v žádném textovém poli. Tím zabráníme skákání Tab po menu/tlačítkách.
        let any_dialog_open = self.show_text_editor
            || self.show_bookmarks
            || self.show_settings
            || self.show_about
            || self.show_update_dialog
            || self.show_rename_single
            || self.show_rename_dialog
            || self.show_search
            || self.show_hash_dialog
            || self.show_new_dir
            || self.show_new_file
            || self.show_select_mask
            || self.show_delete_confirm
            || self.show_copy_confirm
            || self.show_zip_confirm
            || self.show_progress
            || self.pending_action.is_some()
            || self.left.inline_rename_idx.is_some()
            || self.right.inline_rename_idx.is_some();

        let _any_text_open = any_dialog_open;

        if !any_dialog_open {
            ctx.memory_mut(|m| {
                if let Some(focused) = m.focused() {
                    let left_path  = egui::Id::new(("path_input", ActivePanel::Left));
                    let right_path = egui::Id::new(("path_input", ActivePanel::Right));
                    if focused != left_path && focused != right_path {
                        m.surrender_focus(focused);
                    }
                }
            });
        }

        // Maximalizace v prvním framu - with_maximized v NativeOptions
        // nefunguje spolehlivě, ViewportCommand je spolehlivější
        if self.maximize_on_start {
            self.maximize_on_start = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }

        // Ikonka okna (titulek/lišta) v prvním framu - stejně jako
        // maximalizace, with_icon v ViewportBuilder se na některých
        // Linux WM správcích oken nepromítne do dekorace okna
        // spolehlivě, ViewportCommand je spolehlivější.
        if !self.icon_set_on_start {
            self.icon_set_on_start = true;
            if let Ok(icon) = eframe::icon_data::from_png_bytes(
                include_bytes!("../assets/app_icon.png")
            ) {
                ctx.send_viewport_cmd(
                    egui::ViewportCommand::Icon(Some(std::sync::Arc::new(icon)))
                );
            }
        }

        // Automatická kontrola aktualizací - jednou při startu, tiše na pozadí
        if !self.update_auto_checked {
            self.update_auto_checked = true;
            self.check_for_update_silent();
        }

        self.handle_shortcuts(ctx);

        self.poll_op_progress();
        self.poll_search();
        self.poll_hash();
        self.poll_fs_events();
        self.poll_dir_sizes();
        self.update_watcher();

        // Spustit výpočet velikosti složek pokud bylo požádáno z render_panel
        if self.left.dir_size_needed || self.right.dir_size_needed {
            self.left.dir_size_needed  = false;
            self.right.dir_size_needed = false;
            self.start_dir_size_calc();
        }

        // Verze v title baru
        let title = format!("eR Commander v{}", env!("CARGO_PKG_VERSION"));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));

        self.render_menu_bar(ctx);

        egui::TopBottomPanel::bottom("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                macro_rules! tbtn {
                    ($label:expr) => {
                        ui.button($label)
                    };
                }
                if tbtn!("F2 Přejmenovat").clicked() { self.open_rename_single(); }
                if tbtn!("F3 Editor").clicked()      { self.open_text_editor(); }
                if tbtn!("F4 Ext.editor").clicked()  { self.open_external_editor(); }
                if tbtn!("F5 Kopírovat").clicked()   { self.start_copy(); }
                if tbtn!("F6 Přesunout").clicked()   { self.start_move(); }
                if tbtn!("F7 Nova slozka").clicked() {
                    self.new_dir_name.clear();
                    self.new_dir_error = None;
                    self.show_new_dir = true;
                }
                if tbtn!("F8 Smazat").clicked()      { self.request_delete(); }
                if tbtn!("Alt+F5 ZIP").clicked()      { self.start_zip_pack(); }
                if tbtn!("Alt+F7 Najit").clicked()   { self.open_search_dialog(); }
                if tbtn!("Ctrl+M Vše").clicked() {
                    self.rename_error = None;
                    self.show_rename_dialog = true;
                }
                if tbtn!("Ctrl+N Novy soubor").clicked() {
                    self.new_file_name = "novy_soubor.txt".to_string();
                    self.new_file_error = None;
                    self.show_new_file = true;
                }

                ui.separator();

                // Statistiky výběru aktivního panelu
                let panel = match self.active {
                    ActivePanel::Left  => &self.left,
                    ActivePanel::Right => &self.right,
                };
                let sel_count = panel.selected.len();
                if sel_count > 0 {
                    let sel_size: u64 = panel.selected.iter()
                        .filter_map(|&i| panel.entries.get(i))
                        .map(|e| e.effective_size())
                        .sum();
                    let sel_dirs = panel.selected.iter()
                        .filter_map(|&i| panel.entries.get(i))
                        .filter(|e| e.is_dir)
                        .count();
                    let sel_files = sel_count - sel_dirs;

                    let mut stat = format!("Vybráno: {} souborů", sel_files);
                    if sel_dirs > 0 {
                        stat.push_str(&format!(", {} složek", sel_dirs));
                    }
                    if sel_size > 0 {
                        stat.push_str(&format!("  ({})", format_size(sel_size)));
                    }
                    ui.label(egui::RichText::new(stat)
                        .color(egui::Color32::from_rgb(255, 210, 80)));
                } else {
                    // Bez výběru - zobrazíme celkový obsah panelu
                    let total_files = panel.entries.iter().filter(|e| !e.is_dir).count();
                    let total_dirs  = panel.entries.iter().filter(|e| e.is_dir).count();
                    let total_size: u64 = panel.entries.iter()
                        .filter(|e| !e.is_dir).map(|e| e.size).sum();
                    ui.label(egui::RichText::new(
                        format!("{} souborů, {} složek  ({})",
                            total_files, total_dirs, format_size(total_size)))
                        .color(egui::Color32::GRAY));
                }

                if let Some(status) = &self.op_status {
                    ui.separator();
                    ui.colored_label(status.color(),
                        egui::RichText::new(status.text()).strong());
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.columns(2, |columns| {
                let (left_action,  left_ctx)  = render_panel(&mut columns[0], &mut self.left,  ActivePanel::Left,  &mut self.active, &mut self.drag_src);
                let (right_action, right_ctx) = render_panel(&mut columns[1], &mut self.right, ActivePanel::Right, &mut self.active, &mut self.drag_src);

                if let Some(PanelUiAction::OpenBookmarks) = left_action {
                    self.open_bookmarks(ActivePanel::Left);
                }
                if let Some(PanelUiAction::OpenBookmarks) = right_action {
                    self.open_bookmarks(ActivePanel::Right);
                }

                // Zpracování akce z kontextového menu
                let ctx_act = left_ctx.or(right_ctx);
                if let Some(action) = ctx_act {
                    match action {
                        ContextAction::Copy       => self.start_copy(),
                        ContextAction::Move       => self.start_move(),
                        ContextAction::Rename     => self.open_rename_single(),
                        ContextAction::Delete     => self.request_delete(),
                        ContextAction::NewDir     => {
                            self.new_dir_name.clear();
                            self.new_dir_error = None;
                            self.show_new_dir = true;
                        }
                        ContextAction::NewFile    => {
                            self.new_file_name = "novy_soubor.txt".to_string();
                            self.new_file_error = None;
                            self.show_new_file = true;
                        }
                        ContextAction::Open(p)   => { let _ = open_with_system_app(&p); }
                        ContextAction::Edit(p)   => {
                            // Otevřeme soubor ve vestavěném textovém editoru
                            if let Ok(content) = std::fs::read_to_string(&p) {
                                self.text_editor_path     = p;
                                self.text_editor_content  = content;
                                self.text_editor_modified = false;
                                self.text_editor_jump_start = true;
                                self.show_text_editor     = true;
                            } else {
                                self.op_status = Some(StatusMsg::Error("Soubor nelze otevřít jako text.".to_string()));
                            }
                        }
                        ContextAction::SelectAll  => {
                            let panel = match self.active {
                                ActivePanel::Left  => &mut self.left,
                                ActivePanel::Right => &mut self.right,
                            };
                            panel.selected = (0..panel.entries.len()).collect();
                            self.start_dir_size_calc();
                        }
                        ContextAction::DeselectAll => {
                            let panel = match self.active {
                                ActivePanel::Left  => &mut self.left,
                                ActivePanel::Right => &mut self.right,
                            };
                            panel.selected.clear();
                        }
                        ContextAction::Hash => self.open_hash_dialog(),
                    }
                }
            });
        });

        self.render_delete_dialog(ctx);
        self.render_copy_confirm_dialog(ctx);
        self.render_zip_confirm_dialog(ctx);
        self.render_rename_dialog(ctx);
        self.render_overwrite_dialog(ctx);
        self.render_progress_dialog(ctx);
        self.render_search_dialog(ctx);
        self.render_hash_dialog(ctx);
        self.render_about_dialog(ctx);
        self.render_settings_dialog(ctx);
        self.render_bookmarks_dialog(ctx);
        self.render_rename_single_dialog(ctx);
        self.render_select_mask_dialog(ctx);
        self.render_new_dir_dialog(ctx);
        self.render_new_file_dialog(ctx);
        self.render_text_editor(ctx);
        self.poll_update_check();
        self.render_update_dialog(ctx);

        // Barevné téma
        if self.dark_mode {
            ctx.set_visuals(egui::Visuals::dark());
        } else {
            ctx.set_visuals(egui::Visuals::light());
        }

        // Uložíme stav jen tehdy, když se cesta v některém panelu skutečně
        // změnila - ne při každém překreslení (to by zbytečně zatěžovalo disk).
        if self.left.current_path != self.last_saved_left || self.right.current_path != self.last_saved_right {
            self.last_saved_left = self.left.current_path.clone();
            self.last_saved_right = self.right.current_path.clone();
            self.save_state();
        }

        // Ukládáme stav okna každý frame jen pokud se změnil
        // (egui nás informuje přes viewport info)
        self.save_window_state(ctx);

        // "Úklid" fokusu: mimo otevřené dialogy smí mít fokus jen adresní
        // řádka (kvůli psaní cesty). Cokoliv jiného (tlačítko, combobox...)
        // fokus po tomhle snímku ztratí - i kdyby si ho stihlo "ukrást"
        // vestavěnou navigací egui. Díky tomu šipky/Enter/Space spolehlivě
        // patří jen seznamu souborů, nikdy ne tlačítkům okolo.
        if self.op_rx.is_some() || self.search_rx.is_some() || self.dir_size_rx.is_some() {
            ctx.request_repaint();
        }

        // Reset cursor_moved KONEC framu - scroll proběhl, příští frame scrollovat nebudeme
        self.left.cursor_moved  = false;
        self.right.cursor_moved = false;
    }
}

impl FileManagerApp {
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let editing_path = {
            // Zjistíme jestli má fokus adresní řádka TOHOTO framu přes egui memory.
            // Vlastní panel.path_focused je o frame pozadu a způsobuje, že
            // první stisk klávesy po kliknutí na adresní řádku se ztratí.
            let left_id  = egui::Id::new(("path_input", ActivePanel::Left));
            let right_id = egui::Id::new(("path_input", ActivePanel::Right));
            ctx.memory(|m| {
                m.focused().map(|f| f == left_id || f == right_id).unwrap_or(false)
            })
        };

        // ── Enter/Esc pro dialogy - PŘED globálním consume_key blokem ───
        // Klíčové pravidlo: Enter/Esc spotřebujeme POUZE pro ten dialog
        // který je právě otevřený. Ostatní dialogy je nesmí dostat.
        // Pořadí odpovídá prioritě (jen jeden dialog bývá otevřený najednou).

        if self.show_delete_confirm {
            let left  = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft));
            let right = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight));
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if left  { self.delete_focus = self.delete_focus.saturating_sub(1); }
            if right { self.delete_focus = (self.delete_focus + 1).min(1); }
            if enter {
                if self.delete_focus == 0 { self.confirm_delete(); }
                else { self.show_delete_confirm = false; self.delete_targets.clear(); }
            }
            if esc   { self.show_delete_confirm = false; self.delete_targets.clear(); }
        } else if self.show_zip_confirm {
            let zip_id = egui::Id::new("zip_name_field");
            let zip_has_focus = ctx.memory(|m| m.focused() == Some(zip_id));
            if !zip_has_focus {
                let left  = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft));
                let right = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight));
                if left  { self.zip_focus = self.zip_focus.saturating_sub(1); }
                if right { self.zip_focus = (self.zip_focus + 1).min(1); }
            }
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if enter {
                if zip_has_focus || self.zip_focus == 0 { self.confirm_zip_pack(); }
                else { self.show_zip_confirm = false; self.zip_pack_files.clear(); }
            }
            if esc { self.show_zip_confirm = false; self.zip_pack_files.clear(); }
        } else if self.show_copy_confirm {
            let rename_id = egui::Id::new("copy_rename_field");
            let rename_has_focus = ctx.memory(|m| m.focused() == Some(rename_id));
            if !rename_has_focus {
                let left  = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft));
                let right = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight));
                if left  { self.copy_focus = self.copy_focus.saturating_sub(1); }
                if right { self.copy_focus = (self.copy_focus + 1).min(1); }
            }
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if enter && !rename_has_focus {
                if self.copy_focus == 0 { self.confirm_copy_move(); }
                else { self.show_copy_confirm = false; self.copy_targets.clear(); }
            }
            // Enter v přejmenovacím poli = potvrdit kopírování
            if enter && rename_has_focus { self.confirm_copy_move(); }
            if esc   { self.show_copy_confirm = false; self.copy_targets.clear(); }
        } else if self.op_err_wait.is_some() {
            let left  = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft));
            let right = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight));
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if left  { self.op_err_focus = self.op_err_focus.saturating_sub(1); }
            if right { self.op_err_focus = (self.op_err_focus + 1).min(2); }
            if enter { self.resolve_op_error(); }
            if esc   { self.op_err_focus = 2; self.resolve_op_error(); }
        } else if self.pending_action.is_some() {
            // Overwrite dialog - šipky vybírají tlačítko, Enter potvrdí, Esc zruší
            let left  = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft));
            let right = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight));
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if left  { self.overwrite_focus = self.overwrite_focus.saturating_sub(1); }
            if right { self.overwrite_focus = (self.overwrite_focus + 1).min(2); }
            if enter {
                match self.overwrite_focus {
                    0 => self.resolve_pending(true),
                    1 => self.resolve_pending(false),
                    _ => self.cancel_pending(),
                }
            }
            if esc { self.cancel_pending(); }
        } else if self.show_select_mask {
            // Select mask: Enter = potvrdit, Esc = zrušit
            // (textové pole dostane Enter/Esc přes egui automaticky,
            // ale musíme je taky consume, ať se nedostanou do consume bloku níže)
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if enter {
                let add = self.select_mask_add;
                self.apply_mask(add);
                self.show_select_mask = false;
            }
            if esc { self.show_select_mask = false; }
        } else if self.show_rename_single {
            // F2 přejmenování: Enter = potvrdit (přes textové pole), Esc = zrušit
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if enter { self.apply_rename_single(); }
            if esc   { self.show_rename_single = false; }
        } else if self.show_progress && self.op_rx.is_none() {
            // Hotový progress: Enter/Esc zavřou dialog
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if enter || esc { self.show_progress = false; }
        } else if self.pending_action.is_some() {
            let esc = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if esc { self.cancel_pending(); }
        } else if self.show_rename_dialog {
            let esc = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if esc { self.show_rename_dialog = false; self.rename_error = None; }
        } else if self.show_search {
            let esc = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if esc { self.show_search = false; }
        } else if self.show_new_dir {
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if enter { self.confirm_new_dir(); }
            if esc   { self.show_new_dir = false; self.new_dir_error = None; }
        } else if self.show_new_file {
            let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            let esc   = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if enter { self.confirm_new_file(); }
            if esc   { self.show_new_file = false; self.new_file_error = None; }
        } else if self.show_bookmarks {
            let esc = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if esc { self.show_bookmarks = false; }
        } else if self.show_hash_dialog {
            let esc = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if esc {
                if let Some(f) = &self.hash_flag { f.store(OP_STOP, Ordering::Relaxed); }
                self.show_hash_dialog = false;
                self.hash_flag = None;
            }
        }

        // ── Navigační klávesy - spotřebuj VŽDY ──────────────────────────
        // Tím zabráníme egui widget-navigation (Tab/šipky po tlačítkách).
        // Enter spotřebujeme jen pokud není otevřený žádný dialog s textovým polem.
        let any_text_dialog = self.show_rename_single || self.show_select_mask
            || self.show_rename_dialog || self.show_search
            || self.show_new_dir || self.show_new_file
            || self.show_text_editor
            || self.left.inline_rename_idx.is_some()
            || self.right.inline_rename_idx.is_some();

        let nav_free = !any_text_dialog && !editing_path;

        let (tab, up, down, page_up, page_down, home, end, space, enter, del, f2) =
            ctx.input_mut(|i| {
                (
                    false, // Tab zpracován v update() před handle_shortcuts
                    if nav_free { i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)   } else { false },
                    if nav_free { i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) } else { false },
                    if nav_free { i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp)    } else { false },
                    if nav_free { i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown)  } else { false },
                    if nav_free { i.consume_key(egui::Modifiers::NONE, egui::Key::Home)      } else { false },
                    if nav_free { i.consume_key(egui::Modifiers::NONE, egui::Key::End)       } else { false },
                    if nav_free { i.consume_key(egui::Modifiers::NONE, egui::Key::Space)     } else { false },
                    if !any_text_dialog {
                        i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    } else { false },
                    if nav_free { i.consume_key(egui::Modifiers::NONE, egui::Key::Delete)    } else { false },
                    false,
                )
            });

        // F klávesy čteme PŘED navigačním consume blokem přes key_pressed
        // (consume_key s Modifiers::NONE může selhat na NB s Fn vrstvou)
        let (f5, f6, f7, f8, ctrl_m, alt_f7, f2_key, ctrl_n, ctrl_e) = ctx.input(|i| {
            (
                !i.modifiers.alt && i.key_pressed(egui::Key::F5), // F5 bez Alt
                i.key_pressed(egui::Key::F6),
                !i.modifiers.alt && i.key_pressed(egui::Key::F7), // F7 bez Alt
                i.key_pressed(egui::Key::F8),
                i.modifiers.ctrl  && i.key_pressed(egui::Key::M),
                i.modifiers.alt   && i.key_pressed(egui::Key::F7), // Alt+F7
                i.key_pressed(egui::Key::F2),
                i.modifiers.ctrl  && i.key_pressed(egui::Key::N),
                i.modifiers.ctrl  && i.key_pressed(egui::Key::E)
                    || i.key_pressed(egui::Key::F3),
            )
        });
        let f4     = ctx.input(|i| i.key_pressed(egui::Key::F4));
        let alt_f5 = ctx.input(|i| i.modifiers.alt && !i.modifiers.ctrl && i.key_pressed(egui::Key::F5));
        let ctrl_a = ctx.input(|i| i.modifiers.ctrl && !i.modifiers.shift && i.key_pressed(egui::Key::A));
        let alt_c  = ctx.input(|i| i.modifiers.alt && !i.modifiers.ctrl && i.key_pressed(egui::Key::C));
        let ctrl_h = ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::H));
        let f2 = f2 || f2_key;

        // ── F klávesy ────────────────────────────────────────────────────
        // Blokovány jen při psaní nebo aktivní operaci.
        // Hotový progress dialog F klávesy NEBLOKUJE.
        let dialog_open = self.show_delete_confirm
            || self.show_copy_confirm
            || self.show_zip_confirm
            || self.show_rename_dialog
            || self.pending_action.is_some()
            || self.show_search
            || self.show_hash_dialog
            || self.show_about
            || self.show_settings
            || self.show_bookmarks
            || self.show_rename_single
            || self.show_select_mask
            || self.show_new_dir
            || self.show_new_file
            || self.show_text_editor
            || (self.show_progress && self.op_rx.is_some());

        // F klávesy blokují POUZE dialogy které samy pracují s klávesnicí
        // (textová pole apod.). Progress dialog F klávesy NEBLOKUJE nikdy.
        let fkeys_blocked = editing_path
            || self.show_rename_dialog
            || self.pending_action.is_some()
            || self.show_search
            || self.show_hash_dialog
            || self.show_rename_single;

        let (plus, minus) = ctx.input(|i| {
            // + klávesa: numpad Plus nebo kombinace Shift+= (jak je to na CZ klávesnici)
            // V egui 0.27 neexistuje Key::Plus přímo, detekujeme přes events
            let mut p = false;
            let mut m = false;
            for ev in &i.events {
                if let egui::Event::Text(t) = ev {
                    if t == "+" { p = true; }
                    if t == "-" { m = true; }
                }
            }
            (p, m)
        });

        if !fkeys_blocked {
            if f5 { self.start_copy(); }
            if f6 { self.start_move(); }
            if f7 {
                // F7 = nová složka (jako Total Commander)
                self.new_dir_name = String::new();
                self.new_dir_error = None;
                self.show_new_dir = true;
            }
            if alt_f7 { self.open_search_dialog(); }
            if f8 { self.request_delete(); }
            if f2 { self.open_rename_single(); }
            if ctrl_m { self.rename_error = None; self.show_rename_dialog = true; }
            if ctrl_n {
                self.new_file_name = "novy_soubor.txt".to_string();
                self.new_file_error = None;
                self.show_new_file = true;
            }
            if ctrl_e { self.open_text_editor(); }
            if f4     { self.open_external_editor(); }
            if alt_f5 { self.start_zip_pack(); }
            if ctrl_h { self.open_hash_dialog(); }
            if ctrl_a {
                let panel = self.active_panel_mut();
                panel.selected = (0..panel.entries.len()).collect();
                if !panel.entries.is_empty() { panel.dir_size_needed = true; }
            }
            if alt_c {
                let panel = self.active_panel();
                let path = if let Some(idx) = panel.entry_index_at_cursor() {
                    if let Some(e) = panel.entries.get(idx) {
                        panel.current_path.join(&e.name).display().to_string()
                    } else { panel.current_path.display().to_string() }
                } else { panel.current_path.display().to_string() };
                ctx.output_mut(|o| o.copied_text = path.clone());
                self.op_status = Some(StatusMsg::Info(format!("Zkopírováno: {}", path)));
            }
            if plus {
                self.show_select_mask = true;
                self.select_mask_add  = true;
                if self.select_mask.is_empty() { self.select_mask = "*".to_string(); }
            }
            if minus {
                self.show_select_mask = true;
                self.select_mask_add  = false;
                self.select_mask = "*".to_string(); // odznačit vše = *
            }
        }
        // Tab přepíná panel vždy (i při psaní v adresní řádce) a vždy
        // zruší fokus adresní řádky.
        // Tab je zpracován v update() před voláním handle_shortcuts
        let _ = tab;

        // Navigace v seznamu souborů - jen mimo adresní řádku a dialogy.
        if editing_path || dialog_open {
            return;
        }

        const PAGE_JUMP: isize = 15;

        if del  { self.request_delete(); }
        if up   { self.active_panel_mut().move_cursor(-1); }
        if down { self.active_panel_mut().move_cursor(1); }
        if page_up   { self.active_panel_mut().move_cursor(-PAGE_JUMP); }
        if page_down { self.active_panel_mut().move_cursor(PAGE_JUMP); }
        if home { self.active_panel_mut().move_cursor_to_start(); }
        if end  { self.active_panel_mut().move_cursor_to_end(); }
        if space {
            self.active_panel_mut().toggle_selection_at_cursor();
            self.start_dir_size_calc();
        }
        if enter {
            if let Some(CursorAction::OpenExternal(path)) =
                self.active_panel_mut().activate_cursor()
            {
                let _ = open_with_system_app(&path);
            }
        }
    }

    fn render_delete_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_delete_confirm { return; }
        let mut do_delete = false;
        let mut do_cancel = false;

        egui::Window::new("Opravdu smazat?")
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!("Chystáš se natrvalo smazat {} položek:", self.delete_targets.len()));
                egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                    for path in &self.delete_targets {
                        ui.label(path.display().to_string());
                    }
                });
                ui.separator();
                ui.label(egui::RichText::new("← → šipky volí  |  Enter potvrdí  |  Esc zruší")
                    .small().color(egui::Color32::GRAY));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for (label, idx) in [("🗑 Ano, smazat", 0usize), ("Zrušit", 1)] {
                        let r = ui.button(label);
                        if self.delete_focus == idx {
                            ui.painter().rect_stroke(r.rect, 3.0,
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255,190,60)));
                        }
                        if r.clicked() {
                            if idx == 0 { do_delete = true; } else { do_cancel = true; }
                        }
                    }
                });
            });

        if do_delete { self.confirm_delete(); }
        if do_cancel { self.show_delete_confirm = false; self.delete_targets.clear(); }
    }

    fn render_zip_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_zip_confirm { return; }
        let mut do_pack = false; let mut do_cancel = false;
        let count = self.zip_pack_files.len();
        egui::Window::new(format!("📦 Zabalit {} položek do ZIP", count))
            .collapsible(false).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0,0.0))
            .min_width(380.0).show(ctx, |ui| {
                if count <= 5 {
                    for f in &self.zip_pack_files {
                        ui.label(egui::RichText::new(
                            f.file_name().unwrap_or_default().to_string_lossy()).monospace().small());
                    }
                } else { ui.label(format!("{} souborů/složek", count)); }
                ui.add_space(4.0);
                ui.label("Cíl:");
                ui.label(egui::RichText::new(self.zip_pack_dst.display().to_string()).small().color(egui::Color32::GRAY));
                ui.separator();
                ui.label("Název ZIP souboru:");
                ui.add(egui::TextEdit::singleline(&mut self.zip_pack_name)
                    .desired_width(f32::INFINITY).id(egui::Id::new("zip_name_field")));
                ui.label(egui::RichText::new("← → šipky volí  |  Enter potvrdí  |  Esc zruší")
                    .small().color(egui::Color32::GRAY));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for (label, idx) in [("📦 Zabalit", 0usize), ("Zrušit", 1)] {
                        let r = ui.button(label);
                        if self.zip_focus == idx {
                            ui.painter().rect_stroke(r.rect, 3.0,
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255,190,60)));
                        }
                        if r.clicked() { if idx == 0 { do_pack = true; } else { do_cancel = true; } }
                    }
                });
            });
        if do_pack   { self.confirm_zip_pack(); }
        if do_cancel { self.show_zip_confirm = false; self.zip_pack_files.clear(); }
    }

    fn render_copy_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_copy_confirm { return; }
        let kind_label = if self.copy_move_kind == OpKind::Copy { "Kopírovat" } else { "Přesunout" };
        let title = format!("{} {} položek?", kind_label, self.copy_targets.len());
        let mut do_confirm = false;
        let mut do_cancel  = false;

        egui::Window::new(title)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .min_width(360.0)
            .show(ctx, |ui| {
                ui.label("Zdroj:");
                egui::ScrollArea::vertical().max_height(100.0)
                    .id_source("copy_src_scroll")
                    .show(ui, |ui| {
                    for path in &self.copy_targets {
                        ui.label(path.display().to_string());
                    }
                });
                ui.add_space(4.0);
                ui.label("Cíl:");
                ui.label(egui::RichText::new(self.copy_dst.display().to_string()).strong());

                // Přejmenování - jen při jednom souboru
                if self.copy_targets.len() == 1 {
                    ui.add_space(4.0);
                    ui.separator();
                    ui.label("Název v cíli (můžeš přejmenovat):");
                    ui.add(egui::TextEdit::singleline(&mut self.copy_new_name)
                        .id(egui::Id::new("copy_rename_field"))
                        .desired_width(f32::INFINITY));
                }
                ui.separator();
                ui.label(egui::RichText::new("← → šipky volí  |  Enter potvrdí  |  Esc zruší")
                    .small().color(egui::Color32::GRAY));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let confirm_label = format!("Ano, {}", kind_label.to_lowercase());
                    for (label, idx) in [(confirm_label.as_str(), 0usize), ("Zrušit", 1)] {
                        let r = ui.button(label);
                        if self.copy_focus == idx {
                            ui.painter().rect_stroke(r.rect, 3.0,
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255,190,60)));
                        }
                        if r.clicked() {
                            if idx == 0 { do_confirm = true; } else { do_cancel = true; }
                        }
                    }
                });
            });

        if do_confirm { self.confirm_copy_move(); }
        if do_cancel  { self.show_copy_confirm = false; self.copy_targets.clear(); }
    }

    fn render_rename_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_rename_dialog {
            return;
        }
        egui::Window::new("Hromadné přejmenování")
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Maska nového názvu (nepovinné):");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.rename_mask)
                        .hint_text("např. [N]_[C].[E]")
                        .desired_width(220.0));
                    if ui.button("[N]").on_hover_text("Vložit token: název bez přípony").clicked() {
                        self.rename_mask.push_str("[N]");
                    }
                    if ui.button("[E]").on_hover_text("Vložit token: přípona").clicked() {
                        self.rename_mask.push_str("[E]");
                    }
                    if ui.button("[C]").on_hover_text("Vložit token: počítadlo").clicked() {
                        self.rename_mask.push_str("[C]");
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Počítadlo – od:");
                    ui.add(egui::DragValue::new(&mut self.rename_counter_start).speed(1));
                    ui.label("krok:");
                    ui.add(egui::DragValue::new(&mut self.rename_counter_step).speed(1));
                    ui.label("šířka:");
                    ui.add(egui::DragValue::new(&mut self.rename_counter_width).range(1..=8));
                });

                ui.separator();

                ui.label("Najít:");
                ui.text_edit_singleline(&mut self.rename_pattern);
                ui.label("Nahradit za:");
                ui.text_edit_singleline(&mut self.rename_replacement);
                ui.checkbox(&mut self.rename_use_regex, "Použít regulární výraz");

                ui.horizontal(|ui| {
                    ui.label("Písmena:");
                    ui.radio_value(&mut self.rename_case, RenameCase::None, "beze změny");
                    ui.radio_value(&mut self.rename_case, RenameCase::Upper, "VELKÁ");
                    ui.radio_value(&mut self.rename_case, RenameCase::Lower, "malá");
                    ui.radio_value(&mut self.rename_case, RenameCase::Capitalize, "Capitalize");
                });

                ui.separator();
                ui.label("Náhled:");
                match self.rename_preview() {
                    Ok(preview) => {
                        egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                            for (old_name, new_name) in &preview {
                                if old_name == new_name {
                                    ui.label(format!("  {}", old_name));
                                } else {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(120, 200, 120),
                                        format!("{}  →  {}", old_name, new_name),
                                    );
                                }
                            }
                        });
                        self.rename_error = None;
                    }
                    Err(e) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), &e);
                    }
                }

                if let Some(err) = &self.rename_error {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Provést").clicked() {
                        self.apply_bulk_rename();
                        if self.rename_error.is_none() {
                            self.show_rename_dialog = false;
                            self.rename_pattern.clear();
                            self.rename_replacement.clear();
                            self.rename_mask.clear();
                        }
                    }
                    if ui.button("Zrušit").clicked() {
                        self.show_rename_dialog = false;
                        self.rename_error = None;
                    }
                });
            });
    }

    fn render_overwrite_dialog(&mut self, ctx: &egui::Context) {
        if self.pending_action.is_none() {
            return;
        }

        // Pomocná closure pro tlačítko se zvýrazněním
        let mut overwrite = false;
        let mut skip      = false;
        let mut cancel    = false;

        egui::Window::new("Cíl už obsahuje stejně pojmenované soubory")
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} položek v cíli už existuje a budou přepsány, pokud zvolíš \"Přepsat vše\":",
                    self.pending_conflicts.len()
                ));
                egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                    for name in &self.pending_conflicts {
                        ui.label(name);
                    }
                });
                ui.separator();

                // Nápověda ke klávesám
                ui.label(egui::RichText::new("← → šipky volí tlačítko  |  Enter potvrdí  |  Esc zruší")
                    .small().color(egui::Color32::GRAY));
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    let btns = [
                        ("⚠ Přepsat vše", 0usize),
                        ("⏭ Přeskočit existující", 1),
                        ("Zrušit", 2),
                    ];
                    for (label, idx) in btns {
                        let r = ui.button(label);
                        // Zvýrazníme oranžovým rámečkem aktivní tlačítko
                        if self.overwrite_focus == idx {
                            ui.painter().rect_stroke(
                                r.rect,
                                3.0,
                                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 190, 60)),
                            );
                        }
                        if r.clicked() {
                            match idx {
                                0 => overwrite = true,
                                1 => skip      = true,
                                _ => cancel    = true,
                            }
                        }
                    }
                });
            });

        if overwrite { self.resolve_pending(true); }
        if skip      { self.resolve_pending(false); }
        if cancel    { self.cancel_pending(); }
    }

    /// Horní menu - "Příkazy" se všemi funkcemi appky na jednom místě,
    /// "Nápověda" -> "O programu".
    fn render_menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("Příkazy", |ui| {
                    egui::Grid::new("cmd_grid").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                        macro_rules! cmd {
                            ($label:expr, $key:expr, $action:expr) => {
                                if ui.button($label).clicked() { $action; ui.close_menu(); }
                                ui.label(egui::RichText::new($key).weak());
                                ui.end_row();
                            }
                        }
                        cmd!("Přejmenovat",          "F2",      self.open_rename_single());
                        cmd!("Editovat text",         "F3",      self.open_text_editor());
                        cmd!("Ext. editor",           "F4",      self.open_external_editor());
                        cmd!("Kopírovat",             "F5",      self.start_copy());
                        cmd!("Přesunout",             "F6",      self.start_move());
                        cmd!("Nová složka",           "F7",      { self.new_dir_name.clear(); self.new_dir_error = None; self.show_new_dir = true; });
                        cmd!("Smazat",                "F8",      self.request_delete());
                        ui.separator(); ui.separator(); ui.end_row();
                        cmd!("Zabalit do ZIP",        "Alt+F5",  self.start_zip_pack());
                        cmd!("Najít soubory/text",    "Alt+F7",  self.open_search_dialog());
                        cmd!("Hromadné přejmenování", "Ctrl+M",  { self.rename_error = None; self.show_rename_dialog = true; });
                        cmd!("Kontrolní součet",      "Ctrl+H",  self.open_hash_dialog());
                        cmd!("Nový soubor",           "Ctrl+N",  { self.new_file_name = "novy_soubor.txt".to_string(); self.new_file_error = None; self.show_new_file = true; });
                        ui.separator(); ui.separator(); ui.end_row();
                        cmd!("Přepnout panel",        "Tab",     { self.active = match self.active { ActivePanel::Left => ActivePanel::Right, ActivePanel::Right => ActivePanel::Left }; });
                    });
                });

                if ui.button("⚙ Nastavení").clicked() {
                    self.show_settings = true;
                }

                ui.menu_button("Nápověda", |ui| {
                    if ui.button("🔄 Zkontrolovat aktualizace").clicked() {
                        self.check_for_update();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("O programu").clicked() {
                        self.show_about = true;
                        ui.close_menu();
                    }
                });
            });
        });
    }

    /// Dialog hledání souborů podle jména a/nebo textu uvnitř (Alt+F7,
    /// obdoba Total Commanderu).
    fn render_hash_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_hash_dialog {
            return;
        }
        let mut close = false;
        let mut copy_text: Option<String> = None;

        egui::Window::new("Kontrolní součet (MD5 / SHA-256)")
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_width(600.0)
            .show(ctx, |ui| {
                if self.hash_running {
                    ui.label(format!("Počítám {}/{}: {}", self.hash_done, self.hash_total, self.hash_current));
                    ui.add(egui::ProgressBar::new(
                        if self.hash_total > 0 { self.hash_done as f32 / self.hash_total as f32 } else { 0.0 }
                    ));
                    ui.separator();
                }

                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    egui::Grid::new("hash_grid").num_columns(5).striped(true)
                        .spacing([10.0, 4.0]).show(ui, |ui| {
                        ui.label(egui::RichText::new("Soubor").strong());
                        ui.label(egui::RichText::new("MD5").strong());
                        ui.label("");
                        ui.label(egui::RichText::new("SHA-256").strong());
                        ui.label("");
                        ui.end_row();
                        for (name, md5, sha256) in &self.hash_results {
                            ui.label(name);
                            ui.label(egui::RichText::new(md5).monospace().small());
                            if ui.small_button("📋").on_hover_text("Kopírovat MD5 do schránky").clicked() {
                                copy_text = Some(md5.clone());
                            }
                            ui.label(egui::RichText::new(sha256).monospace().small());
                            if ui.small_button("📋").on_hover_text("Kopírovat SHA-256 do schránky").clicked() {
                                copy_text = Some(sha256.clone());
                            }
                            ui.end_row();
                        }
                    });
                });

                if !self.hash_errors.is_empty() {
                    ui.separator();
                    for e in &self.hash_errors {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), e);
                    }
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if self.hash_running {
                        if ui.button("Zrušit").clicked() {
                            if let Some(f) = &self.hash_flag { f.store(OP_STOP, Ordering::Relaxed); }
                        }
                    } else if ui.button("Zavřít").clicked() {
                        close = true;
                    }
                });
            });

        if let Some(t) = copy_text {
            ctx.output_mut(|o| o.copied_text = t);
            self.op_status = Some(StatusMsg::Info("Hash zkopírován do schránky.".to_string()));
        }
        if close {
            self.show_hash_dialog = false;
            self.hash_flag = None;
        }
    }

    fn render_search_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_search {
            return;
        }
        let mut open = true;
        let mut jump_to: Option<PathBuf> = None;

        egui::Window::new("Najít soubory")
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .default_width(480.0)
            .show(ctx, |ui| {
                ui.label("Prohledávaný adresář:");
                let mut root_str = self.search_root.to_string_lossy().into_owned();
                if ui.text_edit_singleline(&mut root_str).changed() {
                    self.search_root = PathBuf::from(root_str);
                }

                ui.add_space(4.0);
                ui.label("Název souboru obsahuje:");
                ui.text_edit_singleline(&mut self.search_name);

                ui.label("Text uvnitř souboru (volitelné, pomalejší):");
                ui.text_edit_singleline(&mut self.search_content);

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Hledat").clicked() {
                        self.start_search();
                    }
                    if self.search_running {
                        ui.spinner();
                        ui.label(format!("prohledáno {} souborů...", self.search_scanned));
                    } else if !self.search_results.is_empty() {
                        ui.label(format!("nalezeno: {}", self.search_results.len()));
                    }
                });

                ui.separator();
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for path in &self.search_results {
                        if ui.selectable_label(false, path.display().to_string()).double_clicked() {
                            jump_to = Some(path.clone());
                        }
                    }
                });
                ui.label("(dvojklik na výsledek tě v panelu přenese přímo k souboru)");
            });

        if !open {
            self.show_search = false;
        }
        if let Some(path) = jump_to {
            self.jump_to_search_result(&path);
            self.show_search = false;
        }
    }

    /// Klasické "O programu" okno - verze, autor, kontakt. Až budeš mít
    /// logo, přidej sem nahoru např. `ui.image(...)` s načteným obrázkem.
    fn render_settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_settings { return; }
        let mut changed = false;

        egui::Window::new("⚙ Nastavení")
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .min_width(320.0)
            .show(ctx, |ui| {
                // Téma
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Vzhled").strong());
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Barevné téma:");
                        let was = self.dark_mode;
                        ui.selectable_value(&mut self.dark_mode, true,  "🌙 Tmavé");
                        ui.selectable_value(&mut self.dark_mode, false, "☀ Světlé");
                        if self.dark_mode != was { changed = true; }
                    });
                });

                ui.add_space(8.0);

                // Řazení
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Řazení složek").strong());
                    ui.separator();
                    let was = self.dir_sort;
                    ui.radio_value(&mut self.dir_sort, DirSort::FirstByName,
                        "Složky vždy nahoře, řazeny dle jména");
                    ui.radio_value(&mut self.dir_sort, DirSort::FirstByCol,
                        "Složky vždy nahoře, řazeny dle zvoleného sloupce");
                    ui.radio_value(&mut self.dir_sort, DirSort::Mixed,
                        "Složky a soubory smíchány dle zvoleného sloupce");
                    if self.dir_sort != was {
                        changed = true;
                        self.left.dir_sort  = self.dir_sort;
                        self.right.dir_sort = self.dir_sort;
                        self.left.refresh();
                        self.right.refresh();
                    }
                });

                ui.add_space(8.0);

                // Zobrazení souborů
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Zobrazení souborů").strong());
                    ui.separator();
                    let was = self.show_hidden;
                    ui.checkbox(&mut self.show_hidden, "Zobrazit skryté a systémové soubory");
                    if self.show_hidden != was {
                        changed = true;
                        self.left.show_hidden  = self.show_hidden;
                        self.right.show_hidden = self.show_hidden;
                        self.left.refresh();
                        self.right.refresh();
                    }
                });

                ui.add_space(8.0);

                // Externí editor
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Externí editor (F4)").strong());
                    ui.separator();
                    ui.label("Cesta k exe souboru editoru:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.external_editor)
                            .desired_width(280.0)
                            .hint_text("např. C:\\Program Files\\Notepad++\\notepad++.exe"));
                        if ui.button("📂").on_hover_text("Vybrat exe soubor").clicked() {
                            #[cfg(windows)]
                            {
                                let result = std::process::Command::new("powershell")
                                    .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command",
                                        "Add-Type -AssemblyName System.Windows.Forms; \
                                         $f = New-Object System.Windows.Forms.OpenFileDialog; \
                                         $f.Filter = 'Spustitelne soubory (*.exe)|*.exe|Vsechny soubory (*.*)|*.*'; \
                                         $f.Title = 'Vybrat editor'; \
                                         if ($f.ShowDialog() -eq 'OK') { Write-Output $f.FileName }"])
                                    .output();
                                if let Ok(out) = result {
                                    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                                    if !path.is_empty() {
                                        self.external_editor = path;
                                        changed = true;
                                    }
                                }
                            }
                            #[cfg(target_os = "linux")]
                            {
                                // Zkusíme zenity (GTK/GNOME), pak kdialog (KDE) jako fallback
                                let zenity = std::process::Command::new("zenity")
                                    .args(["--file-selection", "--title=Vybrat editor"])
                                    .output();
                                let picked = match zenity {
                                    Ok(out) if out.status.success() => {
                                        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
                                    }
                                    _ => {
                                        let kdialog = std::process::Command::new("kdialog")
                                            .args(["--getopenfilename", ".", "*", "--title", "Vybrat editor"])
                                            .output();
                                        match kdialog {
                                            Ok(out) if out.status.success() => {
                                                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
                                            }
                                            _ => None,
                                        }
                                    }
                                };
                                match picked {
                                    Some(path) if !path.is_empty() => {
                                        self.external_editor = path;
                                        changed = true;
                                    }
                                    Some(_) => {}
                                    None => {
                                        self.op_status = Some(StatusMsg::Error(
                                            "Nepodařilo se otevřít dialog pro výběr souboru (chybí zenity nebo kdialog).".to_string()));
                                    }
                                }
                            }
                        }
                    });
                    ui.label(egui::RichText::new(
                        "Tip: notepad.exe, notepad++.exe, code.exe, sublime_text.exe ...")
                        .small().color(egui::Color32::GRAY));
                    if ui.button("💾 Uložit cestu editoru").clicked() {
                        changed = true;
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                if ui.button("Zavřít").clicked() {
                    self.show_settings = false;
                }
            });

        if changed { self.save_state(); }
    }

    fn render_about_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut open = true;
        egui::Window::new("O programu")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    // Ikona aplikace
                    ui.add(
                        egui::Image::from_bytes(
                            "bytes://app_icon",
                            include_bytes!("../assets/app_icon.png").as_ref(),
                        )
                        .fit_to_exact_size(egui::vec2(96.0, 96.0))
                    );
                    ui.add_space(8.0);
                    ui.heading("eR Commander");
                    ui.label(format!("Verze {}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(8.0);
                    ui.label(format!("Autor: {}", APP_AUTHOR));
                    ui.label(format!("Kontakt: {}", APP_CONTACT));
                    ui.add_space(8.0);
                    ui.label("Postaveno v Rustu pomoci egui");
                    ui.add_space(4.0);
                    ui.separator();
                    ui.label(egui::RichText::new("GNU General Public License v3.0")
                        .small().color(egui::Color32::from_rgb(150, 200, 150)));
                    if ui.link("github.com/DaTTcz/eR-Commander").clicked() {
                        let _ = open_with_system_app(
                            &std::path::PathBuf::from("https://github.com/DaTTcz/eR-Commander"));
                    }
                    ui.add_space(4.0);
                    ui.separator();
                    ui.label(egui::RichText::new(
                        format!("Stav: {}", state_file_path().display()))
                        .small().color(egui::Color32::GRAY));
                });
            });
        if !open {
            self.show_about = false;
        }
    }

    /// Dialog správy záložek (hvězdička u přepínače disků) - přidání
    /// aktuální složky, přechod na uloženou, odebrání.
    fn render_bookmarks_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_bookmarks { return; }
        let mut open       = true;
        let mut jump_idx:   Option<usize> = None;
        let mut remove_idx: Option<usize> = None;
        let mut move_up:    Option<usize> = None;
        let mut move_down:  Option<usize> = None;
        let mut edit_idx:   Option<usize> = None;
        let mut save_edit   = false;
        let mut cancel_edit = false;

        egui::Window::new("★ Záložky")
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .default_width(520.0)
            .resizable(true)
            .show(ctx, |ui| {
                // Toolbar - přidání záložek
                ui.horizontal(|ui| {
                    if ui.button("➕ Aktivní + neaktivní panel").on_hover_text(
                        "Uloží oba panely jako dvojitou záložku\n(aktivní = ten ze kterého jsi otevřel záložky)").clicked() {
                        self.add_bookmark_current();
                    }
                    if ui.button("➕ Jen aktivní panel").on_hover_text(
                        "Uloží jen aktivní panel jako jednoduchou záložku").clicked() {
                        self.add_bookmark_single();
                    }
                });
                ui.separator();

                if self.bookmarks.is_empty() {
                    ui.label("Žádné záložky – přidej je tlačítky výše.");
                }

                let count = self.bookmarks.len();
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    for i in 0..count {
                        let is_editing = self.bm_edit_idx == Some(i);

                        if is_editing {
                            // ── Inline editace ─────────────────────────────
                            ui.group(|ui| {
                                ui.label(egui::RichText::new("Upravit záložku").strong());
                                ui.horizontal(|ui| {
                                    ui.label("Název:");
                                    ui.add(egui::TextEdit::singleline(&mut self.bm_edit_name)
                                        .desired_width(300.0));
                                });
                                ui.horizontal(|ui| {
                                    ui.label("A:"); // Aktivní
                                    ui.add(egui::TextEdit::singleline(&mut self.bm_edit_left)
                                        .desired_width(360.0));
                                    if ui.small_button("◎").on_hover_text("Použít aktuální aktivní panel").clicked() {
                                        self.bm_edit_left = match self.bookmarks_target {
                                            ActivePanel::Left  => self.left.current_path.display().to_string(),
                                            ActivePanel::Right => self.right.current_path.display().to_string(),
                                        };
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("N:"); // Neaktivní
                                    ui.add(egui::TextEdit::singleline(&mut self.bm_edit_right)
                                        .desired_width(360.0));
                                    if ui.small_button("◎").on_hover_text("Použít aktuální neaktivní panel").clicked() {
                                        self.bm_edit_right = match self.bookmarks_target {
                                            ActivePanel::Left  => self.right.current_path.display().to_string(),
                                            ActivePanel::Right => self.left.current_path.display().to_string(),
                                        };
                                    }
                                });
                                ui.label(egui::RichText::new("Pravý panel nech prázdný pro jednoduchou záložku.")
                                    .small().color(egui::Color32::GRAY));
                                ui.horizontal(|ui| {
                                    if ui.button("💾 Uložit").clicked() { save_edit = true; }
                                    if ui.button("Zrušit").clicked()    { cancel_edit = true; }
                                });
                            });
                        } else {
                            // ── Normální zobrazení ──────────────────────────
                            ui.horizontal(|ui| {
                                // Přejít
                                if ui.button("▶").on_hover_text("Přejít (Enter)").clicked() {
                                    jump_idx = Some(i);
                                }
                                // Pořadí
                                ui.vertical(|ui| {
                                    if ui.small_button("▲").on_hover_text("Posunout výše").clicked()  { move_up   = Some(i); }
                                    if ui.small_button("▼").on_hover_text("Posunout níže").clicked()  { move_down = Some(i); }
                                });
                                // Obsah záložky
                                ui.vertical(|ui| {
                                    ui.label(egui::RichText::new(&self.bookmarks[i].name).strong());
                                    ui.label(egui::RichText::new(
                                        format!("A: {}", self.bookmarks[i].left.display())).small()
                                        .color(egui::Color32::from_rgb(150, 220, 150)));
                                    if let Some(ref r) = self.bookmarks[i].right.clone() {
                                        ui.label(egui::RichText::new(
                                            format!("N: {}", r.display())).small()
                                            .color(egui::Color32::from_rgb(180, 180, 255)));
                                    }
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.button("🗑").on_hover_text("Smazat záložku").clicked() {
                                        remove_idx = Some(i);
                                    }
                                    if ui.button("✏").on_hover_text("Upravit").clicked() {
                                        edit_idx = Some(i);
                                    }
                                });
                            });
                        }
                        ui.separator();
                    }
                });

                // Enter pro přejití na první záložku pokud nic není editováno
                if self.bm_edit_idx.is_none() {
                    if ctx.input(|i| i.key_pressed(egui::Key::Enter)) && !self.bookmarks.is_empty() {
                        jump_idx = Some(0);
                    }
                }
            });

        // Zpracování akcí
        if !open { self.show_bookmarks = false; self.bm_edit_idx = None; }

        if let Some(idx) = edit_idx {
            let bm = &self.bookmarks[idx];
            self.bm_edit_idx   = Some(idx);
            self.bm_edit_name  = bm.name.clone();
            self.bm_edit_left  = bm.left.display().to_string();
            self.bm_edit_right = bm.right.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        }

        if save_edit {
            if let Some(idx) = self.bm_edit_idx {
                if idx < self.bookmarks.len() {
                    self.bookmarks[idx].name = self.bm_edit_name.trim().to_string();
                    self.bookmarks[idx].left = PathBuf::from(self.bm_edit_left.trim());
                    let right = self.bm_edit_right.trim();
                    self.bookmarks[idx].right = if right.is_empty() { None } else { Some(PathBuf::from(right)) };
                    self.save_state();
                }
            }
            self.bm_edit_idx = None;
        }

        if cancel_edit { self.bm_edit_idx = None; }

        if let Some(idx) = move_up {
            if idx > 0 {
                self.bookmarks.swap(idx, idx - 1);
                self.save_state();
            }
        }
        if let Some(idx) = move_down {
            if idx + 1 < self.bookmarks.len() {
                self.bookmarks.swap(idx, idx + 1);
                self.save_state();
            }
        }

        if let Some(idx) = jump_idx {
            let bm = self.bookmarks[idx].clone();
            self.jump_to_bookmark(&bm);
            self.show_bookmarks = false;
            self.bm_edit_idx = None;
        }
        if let Some(idx) = remove_idx {
            self.remove_bookmark(idx);
            if self.bm_edit_idx == Some(idx) { self.bm_edit_idx = None; }
        }
    }

    fn render_rename_single_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_rename_single {
            return;
        }
        let mut confirm = false;
        let mut cancel = false;

        egui::Window::new("Přejmenovat")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!("Přejmenovat: {}", self.rename_single_old));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.rename_single_new)
                        .desired_width(300.0)
                        .id_source("rename_single_input"),
                );
                response.request_focus();
                // Enter potvrdí přejmenování, Escape zruší.
                let enter = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                let esc   = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                if enter { confirm = true; }
                if esc   { cancel  = true; }
                if let Some(err) = &self.rename_single_error {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        confirm = true;
                    }
                    if ui.button("Zrušit").clicked() {
                        cancel = true;
                    }
                });
            });

        if confirm {
            self.apply_rename_single();
        }
        if cancel {
            self.show_rename_single = false;
        }
    }

    /// Aplikuje masku (glob pattern) na seznam souborů aktivního panelu.
    /// Podporuje * (libovolný řetězec) a ? (libovolný znak).
    fn apply_mask(&mut self, add: bool) {
        let mask = self.select_mask.trim().to_lowercase();
        let panel = match self.active {
            ActivePanel::Left  => &mut self.left,
            ActivePanel::Right => &mut self.right,
        };
        for (i, entry) in panel.entries.iter().enumerate() {
            // Porovnáváme masku s celým názvem (funguje pro složky i soubory).
            // Pro soubory navíc zkoušíme shodu jen se stem (bez přípony),
            // aby maska "*.mkv" nevybírala složky.
            let name_lower = entry.name.to_lowercase();
            let matches = glob_match(&mask, &name_lower);
            if matches {
                if add {
                    if !panel.selected.contains(&i) {
                        panel.selected.push(i);
                    }
                } else {
                    panel.selected.retain(|&x| x != i);
                }
            }
        }
    }

    fn render_select_mask_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_select_mask { return; }
        let add = self.select_mask_add;
        let title = if add { "[+] Označit maskou" } else { "[-] Odznačit maskou" };
        let mut confirm = false;
        let mut cancel  = false;

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Maska (např. *.mkv, film*, *.mp?):");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.select_mask)
                        .desired_width(280.0)
                        .id_source("select_mask_input"),
                );
                resp.request_focus();
                let enter = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                let esc   = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                if enter { confirm = true; }
                if esc   { cancel  = true; }
                ui.horizontal(|ui| {
                    let btn_label = if add { "Označit" } else { "Odznačit" };
                    if ui.button(btn_label).clicked() { confirm = true; }
                    if ui.button("Zrušit").clicked()  { cancel  = true; }
                });
            });

        if confirm {
            self.apply_mask(add);
            self.show_select_mask = false;
        }
        if cancel {
            self.show_select_mask = false;
        }
    }

    fn confirm_new_dir(&mut self) {
        let name = self.new_dir_name.trim().to_string();
        if name.is_empty() {
            self.new_dir_error = Some("Název nesmí být prázdný.".to_string());
            return;
        }
        let path = self.active_panel().current_path.join(&name);
        match fs::create_dir(&path) {
            Ok(()) => {
                self.show_new_dir = false;
                self.new_dir_error = None;
                // Skočíme kurzorem na novou složku
                let panel = self.active_panel_mut();
                panel.refresh();
                if let Some(idx) = panel.entries.iter().position(|e| e.name == name) {
                    panel.cursor = idx + panel.up_offset();
                    panel.cursor_moved = true;
                }
            }
            Err(e) => {
                self.new_dir_error = Some(e.to_string());
            }
        }
    }

    fn confirm_new_file(&mut self) {
        let name = self.new_file_name.trim().to_string();
        if name.is_empty() {
            self.new_file_error = Some("Název nesmí být prázdný.".to_string());
            return;
        }
        let path = self.active_panel().current_path.join(&name);
        // Vytvoříme prázdný soubor s UTF-8 BOM nebo bez - volíme bez BOM
        // (UTF-8 bez BOM je standard na Linuxu a funguje všude)
        match fs::write(&path, b"") {
            Ok(()) => {
                self.show_new_file = false;
                self.new_file_error = None;
                let panel = self.active_panel_mut();
                panel.refresh();
                if let Some(idx) = panel.entries.iter().position(|e| e.name == name) {
                    panel.cursor = idx + panel.up_offset();
                    panel.cursor_moved = true;
                }
            }
            Err(e) => {
                self.new_file_error = Some(e.to_string());
            }
        }
    }

    fn render_new_dir_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_new_dir { return; }
        let mut confirm = false;
        let mut cancel  = false;

        egui::Window::new("Nová složka (F7)")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Název nové složky:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.new_dir_name)
                        .desired_width(280.0)
                        .id_source("new_dir_input"),
                );
                resp.request_focus();
                if let Some(e) = &self.new_dir_error {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), e);
                }
                ui.horizontal(|ui| {
                    if ui.button("Vytvořit").clicked() { confirm = true; }
                    if ui.button("Zrušit").clicked()   { cancel  = true; }
                });
            });

        if confirm { self.confirm_new_dir(); }
        if cancel  { self.show_new_dir = false; self.new_dir_error = None; }
    }

    fn render_new_file_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_new_file { return; }
        let mut confirm = false;
        let mut cancel  = false;

        egui::Window::new("Nový textový soubor (Ctrl+N)")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Název souboru:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.new_file_name)
                        .desired_width(280.0)
                        .id_source("new_file_input"),
                );
                resp.request_focus();
                ui.label(egui::RichText::new("Vytvoří prázdný soubor s UTF-8 kódováním")
                    .small().color(egui::Color32::GRAY));
                if let Some(e) = &self.new_file_error {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), e);
                }
                ui.horizontal(|ui| {
                    if ui.button("Vytvořit").clicked() { confirm = true; }
                    if ui.button("Zrušit").clicked()   { cancel  = true; }
                });
            });

        if confirm { self.confirm_new_file(); }
        if cancel  { self.show_new_file = false; self.new_file_error = None; }
    }

    fn start_zip_pack(&mut self) {
        let files = self.active_panel().effective_paths();
        if files.is_empty() {
            self.op_status = Some(StatusMsg::Warn("Nejsou označeny žádné soubory.".to_string()));
            return;
        }
        let dst_dir = self.inactive_dir();
        self.zip_pack_name = if files.len() == 1 {
            format!("{}.zip", files[0].file_stem().unwrap_or_default().to_string_lossy())
        } else { "archiv.zip".to_string() };
        self.zip_pack_files = files;
        self.zip_pack_dst   = dst_dir;
        self.zip_focus      = 0;
        self.show_zip_confirm = true;
    }

    fn confirm_zip_pack(&mut self) {
        let files   = std::mem::take(&mut self.zip_pack_files);
        let dst_dir = std::mem::take(&mut self.zip_pack_dst);
        let name    = self.zip_pack_name.trim().to_string();
        self.show_zip_confirm = false;
        let name = if name.is_empty() { "archiv.zip".to_string() }
                   else if name.ends_with(".zip") { name }
                   else { format!("{}.zip", name) };
        let zip_path = dst_dir.join(&name);
        let (rx, flag) = spawn_zip_pack(files, zip_path);
        self.start_op("Balím ZIP", rx, flag, 1);
    }

    fn open_external_editor(&mut self) {
        if self.external_editor.trim().is_empty() {
            self.op_status = Some(StatusMsg::Warn(
                "Není nastaven externí editor. Nastavte ho v Nastavení → Editor.".to_string()));
            return;
        }
        let panel = self.active_panel();
        let Some(idx) = panel.entry_index_at_cursor() else { return };
        let Some(entry) = panel.entries.get(idx) else { return };
        if entry.is_dir { return; }
        let file_path = panel.current_path.join(&entry.name);
        let editor = self.external_editor.trim().to_string();
        match std::process::Command::new(&editor)
            .arg(&file_path)
            .spawn()
        {
            Ok(_)  => self.op_status = Some(StatusMsg::Info(format!("Otevřeno v externím editoru: {}", entry.name))),
            Err(e) => self.op_status = Some(StatusMsg::Error(format!("Chyba spuštění editoru: {}", e))),
        }
    }

    fn open_text_editor(&mut self) {
        let panel = self.active_panel();
        let Some(idx) = panel.entry_index_at_cursor() else { return };
        let Some(entry) = panel.entries.get(idx) else { return };
        if entry.is_dir { return; }
        let path = panel.current_path.join(&entry.name);
        match fs::read_to_string(&path) {
            Ok(content) => {
                self.text_editor_path     = path;
                self.text_editor_content  = content;
                self.text_editor_modified = false;
                self.show_text_editor     = true;
                self.text_editor_jump_start = true; // příznak pro reset kurzoru
            }
            Err(e) => {
                self.op_status = Some(StatusMsg::Error(format!("Nelze otevřít: {}", e)));
            }
        }
    }

    fn render_text_editor(&mut self, ctx: &egui::Context) {
        if !self.show_text_editor { return; }

        let title = format!("📝 {}{}",
            self.text_editor_path.file_name()
                .unwrap_or_default().to_string_lossy(),
            if self.text_editor_modified { " •" } else { "" });

        let editor_id = egui::Id::new("text_editor_area");
        let mut close_editor = false;

        egui::Window::new(&title)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_size([700.0, 500.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("💾 Uložit  Ctrl+S").clicked() {
                        self.save_text_editor();
                    }
                    if ui.button("Zavřít  Esc").clicked() {
                        close_editor = true;
                    }
                    if self.text_editor_modified {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 180, 50),
                            "● Neuloženo");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(
                            self.text_editor_path.display().to_string())
                            .small().color(egui::Color32::GRAY));
                    });
                });
                ui.separator();

                let avail = ui.available_size();
                egui::ScrollArea::both().show(ui, |ui| {
                    let resp = ui.add(
                        egui::TextEdit::multiline(&mut self.text_editor_content)
                            .id(editor_id)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(avail.x)
                            .desired_rows(30)
                            .code_editor(),
                    );
                    if resp.changed() {
                        self.text_editor_modified = true;
                    }

                    // Kurzor na začátek při prvním otevření
                    if self.text_editor_jump_start {
                        self.text_editor_jump_start = false;
                        if let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id) {
                            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(0),
                            )));
                            egui::TextEdit::store_state(ctx, editor_id, state);
                        }
                        resp.request_focus();
                    }
                });

                // Ctrl+S uložení, Esc zavření
                let (save, close) = ctx.input(|i| (
                    i.modifiers.ctrl && i.key_pressed(egui::Key::S),
                    i.key_pressed(egui::Key::Escape),
                ));
                if save  { self.save_text_editor(); }
                if close { close_editor = true; }
            });

        if close_editor {
            self.show_text_editor = false;
        }
    }

    fn save_text_editor(&mut self) {
        match fs::write(&self.text_editor_path, self.text_editor_content.as_bytes()) {
            Ok(()) => {
                self.text_editor_modified = false;
                self.op_status = Some(StatusMsg::Info(format!("Uloženo: {}",
                    self.text_editor_path.file_name()
                        .unwrap_or_default().to_string_lossy())));
            }
            Err(e) => {
                self.op_status = Some(StatusMsg::Error(format!("Chyba uložení: {}", e)));
            }
        }
    }

    fn check_for_update_silent(&mut self) {
        // Jako check_for_update ale neotevře dialog - dialog se zobrazí
        // automaticky v poll_update_check pokud najde novější verzi
        if matches!(self.update_state, UpdateState::Checking | UpdateState::Downloading) {
            return;
        }
        self.update_state = UpdateState::Checking;
        // show_update_dialog zůstane false - otevře se jen pokud najdeme update

        let (tx, rx) = std::sync::mpsc::channel();
        self.update_check_rx = Some(rx);

        let repo = GITHUB_REPO.to_string();
        thread::spawn(move || {
            let result = (|| -> Result<UpdateCheckResult, String> {
                let url = format!("https://api.github.com/repos/{}/releases/latest", repo);
                let client = reqwest::blocking::Client::builder()
                    .user_agent("er-commander-updater")
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .map_err(|e| e.to_string())?;
                let resp = client.get(&url).send().map_err(|e| e.to_string())?;
                let json: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
                let version = json["tag_name"].as_str()
                    .ok_or("Nelze přečíst verzi")?.to_string();
                let assets = json["assets"].as_array().ok_or("Žádné assets")?;
                let suffix = platform_release_asset_suffix();
                let asset_url = assets.iter()
                    .find(|a| a["name"].as_str().map(|n| n.ends_with(suffix)).unwrap_or(false))
                    .and_then(|a| a["browser_download_url"].as_str())
                    .ok_or("Žádný release asset pro tuto platformu")?
                    .to_string();
                Ok(UpdateCheckResult { version, download_url: asset_url })
            })();
            let _ = tx.send(result);
        });
    }

    fn check_for_update(&mut self) {
        if matches!(self.update_state, UpdateState::Checking | UpdateState::Downloading) {
            return;
        }
        self.update_state = UpdateState::Checking;
        self.show_update_dialog = true;

        let (tx, rx) = std::sync::mpsc::channel();
        self.update_check_rx = Some(rx);

        let repo = GITHUB_REPO.to_string();
        thread::spawn(move || {
            let result = (|| -> Result<UpdateCheckResult, String> {
                let url = format!("https://api.github.com/repos/{}/releases/latest", repo);
                let client = reqwest::blocking::Client::builder()
                    .user_agent("er-commander-updater")
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .map_err(|e| e.to_string())?;

                let resp = client.get(&url).send().map_err(|e| e.to_string())?;
                let json: serde_json::Value = resp.json().map_err(|e| e.to_string())?;

                let version = json["tag_name"].as_str()
                    .ok_or("Nelze přečíst verzi")?.to_string();

                // Hledáme release asset odpovídající aktuální platformě
                // (Windows = .zip, Linux = .tar.gz)
                let assets = json["assets"].as_array()
                    .ok_or("Žádné assets")?;
                let suffix = platform_release_asset_suffix();
                let asset_url = assets.iter()
                    .find(|a| a["name"].as_str()
                        .map(|n| n.ends_with(suffix)).unwrap_or(false))
                    .and_then(|a| a["browser_download_url"].as_str())
                    .ok_or("Žádný release asset pro tuto platformu")?
                    .to_string();

                Ok(UpdateCheckResult { version, download_url: asset_url })
            })();

            let _ = tx.send(result);
        });
    }

    fn poll_update_check(&mut self) {
        let Some(rx) = self.update_check_rx.take() else { return };

        match rx.try_recv() {
            Ok(Ok(result)) => {
                let current = env!("CARGO_PKG_VERSION");
                let tag = result.version.trim_start_matches('v');
                // Sémantické porovnání - aktualizace jen pokud je GitHub verze VYŠŠÍ
                if is_newer_version(tag, current) {
                    self.update_state = UpdateState::UpdateAvailable {
                        version: result.version,
                        url: result.download_url,
                    };
                    self.show_update_dialog = true;
                } else {
                    self.update_state = UpdateState::UpToDate;
                }
            }
            Ok(Err(e)) => {
                // Chyba - může být z check_for_update nebo z download_and_replace
                self.update_state = UpdateState::UpToDate; // reset
                self.op_status = Some(StatusMsg::Error(format!("Update selhal: {}", e)));
                self.show_update_dialog = false;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.update_check_rx = Some(rx);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.update_state = UpdateState::Idle;
            }
        }
    }

    fn download_and_replace(&mut self) {
        let UpdateState::UpdateAvailable { url, .. } = &self.update_state else { return };
        let url = url.clone();
        self.update_state = UpdateState::Downloading;

        // Použijeme existující update_check_rx pro zpětnou vazbu o chybě
        let (tx, rx) = std::sync::mpsc::channel::<Result<UpdateCheckResult, String>>();
        self.update_check_rx = Some(rx);

        thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let client = reqwest::blocking::Client::builder()
                    .user_agent("er-commander-updater")
                    .timeout(std::time::Duration::from_secs(120))
                    .build()
                    .map_err(|e| format!("HTTP klient: {}", e))?;

                let response = client.get(&url).send()
                    .map_err(|e| format!("Stahování: {}", e))?;

                let status = response.status();
                if !status.is_success() {
                    return Err(format!("HTTP {}", status));
                }

                let bytes = response.bytes()
                    .map_err(|e| format!("Čtení dat: {}", e))?;

                let current_exe = std::env::current_exe()
                    .map_err(|e| format!("Cesta k exe: {}", e))?;

                if cfg!(windows) {
                    // Windows: staženy je .zip obsahující .exe. Rozbalíme
                    // .exe z archivu do .exe.new, spustíme batch skript,
                    // který po ukončení tohoto procesu provede přepis
                    // a znovu spustí aplikaci (běžící .exe nelze na
                    // Windows přepsat přímo).
                    let zip_path = current_exe.with_extension("zip.update");
                    fs::write(&zip_path, &bytes)
                        .map_err(|e| format!("Zápis staženého ZIP: {}", e))?;

                    let zf = File::open(&zip_path)
                        .map_err(|e| format!("Otevření ZIP: {}", e))?;
                    let mut archive = zip::ZipArchive::new(zf)
                        .map_err(|e| format!("Čtení ZIP: {}", e))?;

                    let new_exe = current_exe.with_extension("exe.new");
                    let mut extracted = false;
                    for i in 0..archive.len() {
                        let mut entry = archive.by_index(i)
                            .map_err(|e| format!("Čtení ZIP položky: {}", e))?;
                        if entry.name().to_lowercase().ends_with(".exe") {
                            let mut out = File::create(&new_exe)
                                .map_err(|e| format!("Zápis nového exe: {}", e))?;
                            std::io::copy(&mut entry, &mut out)
                                .map_err(|e| format!("Rozbalení exe: {}", e))?;
                            extracted = true;
                            break;
                        }
                    }
                    let _ = fs::remove_file(&zip_path);
                    if !extracted {
                        return Err("V ZIP archivu nebyl nalezen .exe soubor".to_string());
                    }

                    let bat_path = current_exe.with_extension("update.bat");
                    let current_name = current_exe.to_string_lossy().to_string();
                    let new_name     = new_exe.to_string_lossy().to_string();
                    let bat_content  = format!(
                        "@echo off\r\ntimeout /t 2 /nobreak >nul\r\nmove /y \"{new}\" \"{cur}\"\r\nstart \"\" \"{cur}\"\r\ndel \"%~f0\"",
                        new = new_name, cur = current_name
                    );
                    fs::write(&bat_path, bat_content)
                        .map_err(|e| format!("Zápis bat skriptu: {}", e))?;

                    std::process::Command::new("cmd")
                        .args(["/C", &bat_path.to_string_lossy()])
                        .spawn()
                        .map_err(|e| format!("Spuštění updatéru: {}", e))?;

                    std::process::exit(0);
                } else {
                    // Linux: stažený je .tar.gz obsahující binárku. Na
                    // Linuxu lze spuštěný soubor bezpečně nahradit
                    // přejmenováním - proces si drží starou inode dál
                    // otevřenou, dokud sám neskončí.
                    let tmp_dir = current_exe.parent()
                        .ok_or("Neznámý adresář aplikace")?
                        .join(".er_commander_update_tmp");
                    let _ = fs::remove_dir_all(&tmp_dir);
                    fs::create_dir_all(&tmp_dir)
                        .map_err(|e| format!("Vytvoření dočasného adresáře: {}", e))?;

                    let archive_path = tmp_dir.join("update.tar.gz");
                    fs::write(&archive_path, &bytes)
                        .map_err(|e| format!("Zápis archivu: {}", e))?;

                    let status = std::process::Command::new("tar")
                        .args(["xzf", &archive_path.to_string_lossy(), "-C", &tmp_dir.to_string_lossy()])
                        .status()
                        .map_err(|e| format!("Spuštění tar: {}", e))?;
                    if !status.success() {
                        let _ = fs::remove_dir_all(&tmp_dir);
                        return Err("Rozbalení archivu selhalo".to_string());
                    }

                    let bin_name = current_exe.file_name()
                        .ok_or("Neznámý název binárky")?;
                    let new_bin = tmp_dir.join(bin_name);
                    if !new_bin.exists() {
                        let _ = fs::remove_dir_all(&tmp_dir);
                        return Err("V archivu nebyla nalezena binárka".to_string());
                    }

                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Ok(meta) = fs::metadata(&new_bin) {
                            let mut perms = meta.permissions();
                            perms.set_mode(0o755);
                            let _ = fs::set_permissions(&new_bin, perms);
                        }
                    }

                    fs::rename(&new_bin, &current_exe)
                        .map_err(|e| format!("Nahrazení binárky: {}", e))?;

                    let _ = fs::remove_dir_all(&tmp_dir);

                    std::process::Command::new(&current_exe)
                        .spawn()
                        .map_err(|e| format!("Spuštění nové verze: {}", e))?;

                    std::process::exit(0);
                }
            })();

            if let Err(e) = result {
                // Pošleme chybu zpátky do UI
                let _ = tx.send(Err(e));
            }
        });
    }

    fn render_update_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_update_dialog { return; }
        let mut close = false;

        egui::Window::new("🔄 Aktualizace")
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .collapsible(false)
            .resizable(false)
            .min_width(320.0)
            .show(ctx, |ui| {
                match &self.update_state.clone() {
                    UpdateState::Checking => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Kontroluji aktualizace...");
                        });
                    }
                    UpdateState::UpToDate => {
                        ui.label(format!("✅ Máš nejnovější verzi (v{}).",
                            env!("CARGO_PKG_VERSION")));
                        if ui.button("Zavřít").clicked() { close = true; }
                    }
                    UpdateState::UpdateAvailable { version, .. } => {
                        ui.label(format!("🆕 Dostupná nová verze: {}", version));
                        ui.label(format!("Aktuální verze: v{}", env!("CARGO_PKG_VERSION")));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("⬇ Stáhnout a nainstalovat").clicked() {
                                self.download_and_replace();
                            }
                            if ui.button("Později").clicked() { close = true; }
                        });
                    }
                    UpdateState::Downloading => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Stahuji aktualizaci...");
                        });
                        ui.label("Aplikace se po dokončení automaticky restartuje.");
                    }
                    UpdateState::Idle => { close = true; }
                }
            });

        if close { self.show_update_dialog = false; }
    }
}

/// Jednoduchý glob match: * = libovolný řetězec, ? = libovolný znak.
/// Funguje case-insensitive (volající by měl předat oba argumenty lowercase).
/// Vrátí true pokud je `remote` verze vyšší než `local`.
/// Porovnává sémanticky: major.minor.patch
fn is_newer_version(remote: &str, local: &str) -> bool {
    fn parse(v: &str) -> (u32, u32, u32) {
        let parts: Vec<u32> = v.split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        (parts.get(0).copied().unwrap_or(0),
         parts.get(1).copied().unwrap_or(0),
         parts.get(2).copied().unwrap_or(0))
    }
    parse(remote) > parse(local)
}

fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let mut dp = vec![vec![false; n.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' { dp[i][0] = dp[i - 1][0]; }
    }
    for i in 1..=p.len() {
        for j in 1..=n.len() {
            if p[i - 1] == '*' {
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            } else if p[i - 1] == '?' || p[i - 1] == n[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }
    dp[p.len()][n.len()]
}

fn render_panel(
    ui: &mut egui::Ui,
    panel: &mut Panel,
    which: ActivePanel,
    active: &mut ActivePanel,
    drag_src: &mut Option<ActivePanel>,
) -> (Option<PanelUiAction>, Option<ContextAction>) {
    let is_active = *active == which;
    let mut panel_action: Option<PanelUiAction> = None;
    let mut ctx_action: Option<ContextAction> = None;

    let frame = egui::Frame::none()
        .fill(if is_active {
            // Aktivní panel - světlejší tmavý teal, odlišný od neaktivního pozadí
            {
                let bg = ui.visuals().panel_fill;
                egui::Color32::from_rgb(
                    bg.r().saturating_add(8),
                    bg.g().saturating_add(18),
                    bg.b().saturating_add(25),
                )
            }
        } else {
            ui.visuals().panel_fill
        })
        .inner_margin(4.0);

    frame.show(ui, |ui| {
        // ── Horní lišta ─────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            let label = panel.current_path.components().next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_else(|| "Disk".to_string());
            egui::ComboBox::from_id_source(format!("drives_{:?}", which))
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for drive in panel.drives.clone() {
                        let item = ui.selectable_label(false, &drive);
                        if item.clicked() {
                            *active = which;
                            panel.archive_location = None;
                            panel.current_path = PathBuf::from(&drive);
                            panel.refresh();
                        }
                        item.surrender_focus();
                    }
                });
            let star_w = 28.0;
            let bar_w  = (ui.available_width() - star_w).max(20.0);
            if let Some((free, total)) = disk_space(&panel.current_path) {
                if total > 0 {
                    let used = total - free;
                    let frac = used as f64 / total as f64;
                    let bar = ui.add(egui::ProgressBar::new(frac as f32)
                        .desired_width(bar_w)
                        .text(format!("{} / {} / {}", format_size(free), format_size(used), format_size(total))));
                    bar.on_hover_text(format!("Volné: {}  |  Použité: {}  |  Celkem: {}",
                        format_size(free), format_size(used), format_size(total)));
                } else { ui.add_space(bar_w); }
            } else { ui.add_space(bar_w); }
            if ui.button(Icons::bookmark()).clicked() {
                panel_action = Some(PanelUiAction::OpenBookmarks);
            }
        });

        // ── Adresní řádek ───────────────────────────────────────────────────
        if panel.archive_location.is_none() {
            let mut err: Option<String> = None;
            let mut do_navigate = false;
            let mut breadcrumb_nav: Option<PathBuf> = None;
            let path_id = egui::Id::new(("path_input", which));

            // Breadcrumb – klikací segmenty cesty
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let path = panel.current_path.clone();
                let mut cumulative = PathBuf::new();
                let components: Vec<_> = path.components().collect();
                let mut skip_next_root = false;

                for (i, comp) in components.iter().enumerate() {
                    use std::path::Component;
                    let seg = match comp {
                        Component::Prefix(p) => {
                            let s = p.as_os_str().to_string_lossy().to_string();
                            cumulative.push(comp);
                            // Přidáme hned i RootDir aby cumulative = C:\ (ne jen C:)
                            cumulative.push(Component::RootDir);
                            skip_next_root = true;
                            format!("{}\\", s)
                        }
                        Component::RootDir => {
                            if skip_next_root { skip_next_root = false; continue; }
                            cumulative.push(comp);
                            "\\".to_string()
                        }
                        Component::Normal(n) => {
                            cumulative.push(comp);
                            n.to_string_lossy().to_string()
                        }
                        _ => continue,
                    };

                    let _is_last = i == components.len() - 1
                        || (skip_next_root == false
                            && components.get(i+1).map(|c| matches!(c, Component::Normal(_))).unwrap_or(true)
                            && i == components.len().saturating_sub(1));

                    // Skutečně poslední = aktuální složka
                    let is_current = i == components.len() - 1;

                    // Detekujeme hover z předchozího framu přes egui memory
                    let hover_id = egui::Id::new(("bc_hover", which, i));
                    let was_hovered = ui.ctx().memory(|m| m.data.get_temp::<bool>(hover_id).unwrap_or(false));

                    let color = if is_current {
                        egui::Color32::from_rgb(220, 220, 220)
                    } else if was_hovered {
                        egui::Color32::from_rgb(255, 190, 60) // oranžová při hover
                    } else {
                        egui::Color32::from_rgb(120, 180, 220) // modrá normálně
                    };

                    let r = ui.add(egui::Label::new(
                        egui::RichText::new(&seg).color(color).small()
                    ).sense(if is_current { egui::Sense::hover() } else { egui::Sense::click() }));

                    // Uložíme hover stav pro příští frame
                    let hovered = r.hovered();
                    ui.ctx().memory_mut(|m| m.data.insert_temp(hover_id, hovered));

                    if r.clicked() {
                        breadcrumb_nav = Some(cumulative.clone());
                    }
                    if !is_current {
                        ui.label(egui::RichText::new(" › ").small()
                            .color(egui::Color32::GRAY));
                    }
                }
            });

            ui.horizontal(|ui| {
                let inner = ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn = ui.button("Přejít");
                        btn.surrender_focus();

                        // Pokud pole nemá fokus, zobrazíme ho jako label (klik = navigace)
                        // Pokud fokus má, zobrazíme TextEdit pro editaci
                        let had_focus = ui.memory(|m| m.focused() == Some(path_id));

                        let resp = ui.add(egui::TextEdit::singleline(&mut panel.path_input)
                            .desired_width(ui.available_width())
                            .id(path_id));
                        panel.path_focused = resp.has_focus();

                        // Levé kliknutí bez fokusu = naviguj do té cesty
                        if resp.clicked() && !had_focus {
                            do_navigate = true;
                        }

                        // Pravé tlačítko = označí celý text a ukáže context menu
                        resp.context_menu(|ui| {
                            if ui.button("📋 Kopírovat cestu").clicked() {
                                ui.output_mut(|o| o.copied_text = panel.path_input.clone());
                                ui.close_menu();
                            }
                            if ui.button("✏ Upravit cestu ručně").clicked() {
                                ui.memory_mut(|m| m.request_focus(path_id));
                                ui.close_menu();
                            }
                            if ui.button("➡ Přejít").clicked() {
                                do_navigate = true;
                                ui.close_menu();
                            }
                        });

                        // Při pravém kliknutí označíme celý text
                        if resp.secondary_clicked() {
                            ui.memory_mut(|m| m.request_focus(path_id));
                            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), path_id) {
                                let len = panel.path_input.chars().count();
                                state.cursor.set_char_range(Some(egui::text::CCursorRange {
                                    primary:   egui::text::CCursor::new(0),
                                    secondary: egui::text::CCursor::new(len),
                                }));
                                egui::TextEdit::store_state(ui.ctx(), path_id, state);
                            }
                        }

                        (btn, resp)
                    }).inner;

                // Enter v adresním řádku
                let enter = inner.1.has_focus()
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                if inner.0.clicked() || enter {
                    do_navigate = true;
                }
            });
            if do_navigate {
                *active = which;
                if let Err(e) = panel.navigate_to_input() { err = Some(e); }
            }
            if let Some(path) = breadcrumb_nav {
                *active = which;
                panel.archive_location = None;
                panel.current_path = path;
                panel.refresh();
                panel.cursor = 0;
                panel.cursor_moved = true;
            }
            if let Some(e) = err {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), e);
            }
        } else if let Some(loc) = &panel.archive_location {
            panel.path_focused = false;
            ui.colored_label(egui::Color32::from_rgb(255, 200, 80),
                format!("[ZIP] {} /{}", loc.archive_path.file_name()
                    .unwrap_or_default().to_string_lossy(), loc.internal_dir));
        }
        if let Some(e) = &panel.error {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), e);
        }

        // ── Záhlaví + seznam ────────────────────────────────────────────────
        ui.separator();

        // Pevné šířky sloupců v px
        const ICO_W:  f32 = 20.0;
        const EXT_W:  f32 = 52.0;
        const SIZE_W: f32 = 72.0;
        const DATE_W: f32 = 122.0;
        const ATTR_W: f32 = 24.0;
        const H:      f32 = 18.0;
        // Celková fixní suma + spacing (egui přidává ~4px mezi widgety)
        let fixed = ICO_W + EXT_W + SIZE_W + DATE_W + ATTR_W + 30.0;
        let name_w = (ui.available_width() - fixed).max(80.0);

        // Záhlaví sloupců
        let sort_arrow = |col: SortColumn| if panel.sort_col == col {
            if panel.sort_dir == SortDir::Asc { " ▲" } else { " ▼" }
        } else { "" };
        let mut sort_change: Option<SortColumn> = None;

        ui.horizontal(|ui| {
            ui.add_space(ICO_W);
            macro_rules! hdr {
                ($label:expr, $col:expr, $w:expr) => {{
                    let t = egui::RichText::new(format!("{}{}", $label, sort_arrow($col))).strong();
                    if ui.add_sized([$w, H], egui::Button::new(t).frame(false)).clicked() {
                        sort_change = Some($col);
                    }
                }};
            }
            hdr!("Název",    SortColumn::Name, name_w);
            hdr!("Příp.",    SortColumn::Ext,  EXT_W);
            hdr!("Vel.",     SortColumn::Size, SIZE_W);
            hdr!("Datum",    SortColumn::Date, DATE_W);
            hdr!("A",        SortColumn::Attr, ATTR_W);
        });

        if let Some(col) = sort_change {
            if panel.sort_col == col {
                panel.sort_dir = if panel.sort_dir == SortDir::Asc { SortDir::Desc } else { SortDir::Asc };
            } else {
                panel.sort_col = col;
                panel.sort_dir = SortDir::Asc;
            }
            panel.refresh();
        }
        ui.separator();

        // Zachytíme šířku panelu PŘED ScrollArea - uvnitř ní se available_width
        // mění kvůli scrollbaru a max_rect není spolehlivý.
        let panel_width = ui.available_width();

        // ── Scrollovatelný seznam ────────────────────────────────────────────
        egui::ScrollArea::vertical()
            .id_source(format!("scroll_{:?}", which))
            .show(ui, |ui| {
                // Zakázat hover highlight – kurzor kreslíme sami.
                {
                    let v = ui.visuals_mut();
                    v.widgets.hovered.weak_bg_fill = egui::Color32::TRANSPARENT;
                    v.widgets.hovered.bg_fill      = egui::Color32::TRANSPARENT;
                    v.widgets.hovered.bg_stroke    = egui::Stroke::NONE;
                    v.widgets.active.weak_bg_fill  = egui::Color32::TRANSPARENT;
                }

                let up_offset = panel.up_offset();
                let mut enter_target: Option<String> = None;
                let mut open_zip:     Option<PathBuf> = None;
                let mut go_up = false;

                // Inline rename - zpracujeme PŘED smyčkou
                if let Some(ri) = panel.inline_rename_idx {
                    let old_name = panel.entries.get(ri).map(|e| e.name.clone());
                    let mut done   = false;
                    let mut cancel = false;
                    ui.horizontal(|ui| {
                        ui.add_space(ICO_W);
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut panel.inline_rename_buf)
                                .desired_width(name_w)
                                .id_source((which, "inline_rename")),
                        );
                        resp.request_focus();
                        // Enter a Esc čteme přímo - nezávisí na lost_focus
                        if resp.has_focus() {
                            let enter = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                            let esc   = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                            if enter { done   = true; }
                            if esc   { cancel = true; }
                        }
                        // Fallback: klik jinam = zruší
                        if resp.lost_focus() && !done { cancel = true; }
                    });
                    if done {
                        if let Some(old) = old_name {
                            let new_name = panel.inline_rename_buf.clone();
                            let cur = panel.current_path.clone();
                            panel.inline_rename_idx = None;
                            if !new_name.is_empty() && new_name != old {
                                let _ = fs::rename(cur.join(&old), cur.join(&new_name));
                                panel.refresh();
                            }
                        }
                    }
                    if cancel { panel.inline_rename_idx = None; }
                }
                // context_menu je na row_resp přímo

                // Makro pro jeden řádek tabulky:
                //   icon_text – text ikonky
                //   stem      – název (bez přípony)
                //   ext/size/date/attr – ostatní sloupce
                //   selected, is_cur – stav
                // Vrátí (clicked, double_clicked).
                macro_rules! file_row {
                    ($idx:expr, $icon:expr, $stem:expr, $ext:expr, $size:expr,
                     $date:expr, $attr:expr, $sel:expr, $cur:expr) => {{                        // Pomocná funkce pro buňku s textem doleva a ořezem
                        let cell_left = |ui: &mut egui::Ui, w: f32, text: &str, sel: bool, cur: bool| {
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(w, H), egui::Sense::click());
                            if ui.is_rect_visible(rect) {
                                let bg = if sel { ui.visuals().selection.bg_fill }
                                         else { egui::Color32::TRANSPARENT };
                                if bg != egui::Color32::TRANSPARENT {
                                    ui.painter().rect_filled(rect, 2.0, bg);
                                }
                                let color = if sel { egui::Color32::from_rgb(255, 200, 50) }
                                            else if cur { ui.visuals().strong_text_color() }
                                            else { ui.visuals().text_color() };
                                let mut job = egui::text::LayoutJob::simple_singleline(
                                    text.to_string(),
                                    ui.style().text_styles[&egui::TextStyle::Body].clone(),
                                    color,
                                );
                                job.wrap.max_rows = 1;
                                job.wrap.break_anywhere = true;
                                let galley = ui.fonts(|f| f.layout_job(job));
                                let pos = egui::pos2(rect.min.x + 2.0,
                                    rect.center().y - galley.size().y / 2.0);
                                ui.painter().with_clip_rect(rect).galley(pos, galley, color);
                            }
                            resp
                        };

                        // Pomocná funkce pro buňku s textem doprava
                        let cell_right = |ui: &mut egui::Ui, w: f32, text: &str, sel: bool, cur: bool| {
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(w, H), egui::Sense::click());
                            if ui.is_rect_visible(rect) {
                                let bg = if sel { ui.visuals().selection.bg_fill }
                                         else { egui::Color32::TRANSPARENT };
                                if bg != egui::Color32::TRANSPARENT {
                                    ui.painter().rect_filled(rect, 2.0, bg);
                                }
                                let color = if sel { egui::Color32::from_rgb(255, 200, 50) }
                                            else if cur { ui.visuals().strong_text_color() }
                                            else { ui.visuals().text_color() };
                                let mut job = egui::text::LayoutJob::simple_singleline(
                                    text.to_string(),
                                    ui.style().text_styles[&egui::TextStyle::Body].clone(),
                                    color,
                                );
                                job.wrap.max_rows = 1;
                                job.wrap.break_anywhere = true;
                                let galley = ui.fonts(|f| f.layout_job(job));
                                // Zarovnání doprava
                                let pos = egui::pos2(
                                    rect.max.x - galley.size().x - 2.0,
                                    rect.center().y - galley.size().y / 2.0,
                                );
                                ui.painter().with_clip_rect(rect).galley(pos, galley, color);
                            }
                            resp
                        };

                        let row_resp = ui.horizontal(|ui| {
                            // ikonka
                            ui.add_sized([ICO_W, H], egui::Label::new($icon));
                            // název doleva + ořez
                            cell_left(ui,  name_w, $stem,           $sel, $cur);
                            // přípona doleva
                            cell_left(ui,  EXT_W,  $ext,            $sel, $cur);
                            // velikost doprava
                            let size_str: String = $size;
                            cell_right(ui, SIZE_W, &size_str,       $sel, $cur);
                            // datum doleva
                            cell_left(ui,  DATE_W, $date,           $sel, $cur);
                            // atribut doleva
                            cell_left(ui,  ATTR_W, $attr,           $sel, $cur);
                        });

                        let mut row_rect = row_resp.response.rect;
                        row_rect.max.x = row_rect.min.x + panel_width - 4.0;
                        if $cur {
                            ui.painter().rect_stroke(row_rect, 0.0,
                                egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(255,190,60)));
                            // Scrolluj na kurzor jen pokud se pohnul klávesou
                            if panel.cursor_moved {
                                ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
                            }
                        }
                        let resp = ui.interact(row_rect,
                            egui::Id::new((which, "row", $idx as usize)),
                            egui::Sense::click_and_drag());
                        let clicked  = resp.clicked();
                        let dbl      = resp.double_clicked();
                        let dragging = resp.dragged();
                        (clicked, dbl, row_rect, resp, dragging)
                    }};
                }

                // ".." řádek
                if up_offset == 1 {
                    let is_cur = is_active && panel.cursor == 0;
                    let (cl, _, _, _, _) = file_row!(
                        0usize, Icons::up(), "..", "", String::new(),
                        "", "", false, is_cur
                    );
                    if cl { go_up = true; }
                }

                for (i, entry) in panel.entries.iter().enumerate() {
                    let idx  = i + up_offset;
                    let sel  = panel.selected.contains(&i);
                    let cur  = is_active && idx == panel.cursor;
                    let ico  = if entry.is_dir { Icons::folder() }
                               else if entry.is_archive { Icons::archive() }
                               else { Icons::file() };

                    let stem = if !entry.is_dir && !entry.ext.is_empty() {
                        entry.name.strip_suffix(&format!(".{}", entry.ext))
                            .unwrap_or(&entry.name).to_string()
                    } else {
                        entry.name.clone()
                    };
                    let size_str = if entry.is_dir {
                        match entry.dir_size {
                            Some(s) => format_size(s),
                            None => String::new(),
                        }
                    } else {
                        format_size(entry.size)
                    };

                    // Inline přejmenování – tento řádek přeskočíme pokud je aktivní rename
                    if panel.inline_rename_idx == Some(i) {
                        continue; // TextEdit se renderuje před smyčkou
                    }

                    let (cl, dbl, _row_rect, ctx_resp, is_dragging) = file_row!(
                        idx, ico, &stem, &entry.ext, size_str,
                        &entry.modified_str(), entry.attr_str(),
                        sel, cur
                    );

                    if cl {
                        // Levé tlačítko jen aktivuje panel a posune kurzor
                        // Označování souborů je výhradně přes mezerník
                        *active = which;
                        panel.cursor = idx;
                    }

                    // Drag start - při tažení nastavíme zdroj a označíme soubor
                    if is_dragging {
                        *active = which;
                        *drag_src = Some(which);
                        if !panel.selected.contains(&i) {
                            panel.selected.retain(|_| false);
                            panel.selected.push(i);
                        }
                        // Ghost text u kurzoru myši
                        let sel = panel.selected.len();
                        let ghost = if sel == 1 { format!("➡ {}", entry.name) }
                                    else { format!("➡ {} souborů", sel) };
                        let mouse_pos = ui.input(|inp| inp.pointer.hover_pos().unwrap_or_default());
                        egui::show_tooltip_at(ui.ctx(), ui.layer_id(), egui::Id::new("drag_tooltip"),
                            mouse_pos + egui::vec2(12.0, 8.0), |ui| {
                                ui.label(egui::RichText::new(&ghost)
                                    .color(egui::Color32::from_rgb(255, 210, 80)));
                            });
                    }
                    if dbl {
                        panel.cursor = idx;
                        if entry.is_dir {
                            enter_target = Some(entry.name.clone());
                        } else if entry.is_archive && panel.archive_location.is_none() {
                            let p = panel.current_path.join(&entry.name);
                            if entry.ext == "zip" { open_zip = Some(p); }
                            else { let _ = open_with_system_app(&p); }
                        } else if panel.archive_location.is_none() {
                            let _ = open_with_system_app(
                                &panel.current_path.join(&entry.name));
                        }
                    }

                    // context_menu na ctx_resp z makra (Sense::hover zachytí pravé tlačítko pro context_menu)
                    let entry_name   = entry.name.clone();
                    let entry_is_dir = entry.is_dir;
                    let entry_path   = panel.current_path.join(&entry.name);
                    let in_archive   = panel.archive_location.is_none();
                    ctx_resp.context_menu(|ui| {
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            ui.close_menu();
                        }
                        let sel = panel.selected.len();
                        let label = if sel > 0 {
                            format!("{} označených položek", sel)
                        } else {
                            entry_name.clone()
                        };
                        ui.label(egui::RichText::new(&label).strong());
                        ui.separator();
                        if ui.button("✏ Přejmenovat    F2").clicked() { ctx_action = Some(ContextAction::Rename); ui.close_menu(); }
                        if !entry_is_dir && in_archive {
                            if ui.button("📝 Editovat      F3").clicked() {
                                ctx_action = Some(ContextAction::Edit(entry_path.clone()));
                                ui.close_menu();
                            }
                        }
                        if ui.button("📋 Kopírovat      F5").clicked() { ctx_action = Some(ContextAction::Copy);   ui.close_menu(); }
                        if ui.button("➡ Přesunout      F6").clicked() { ctx_action = Some(ContextAction::Move);   ui.close_menu(); }
                        if ui.button("📁 Nová složka   F7").clicked() { ctx_action = Some(ContextAction::NewDir);  ui.close_menu(); }
                        if ui.button("🗑 Smazat         F8").clicked() { ctx_action = Some(ContextAction::Delete); ui.close_menu(); }
                        if !entry_is_dir {
                            if ui.button("# Kontrolní součet Ctrl+H").clicked() {
                                ctx_action = Some(ContextAction::Hash);
                                ui.close_menu();
                            }
                        }
                        ui.separator();
                        if !entry_is_dir && in_archive {
                            if ui.button("↗ Otevřít").clicked() {
                                ctx_action = Some(ContextAction::Open(entry_path.clone()));
                                ui.close_menu();
                            }
                        }
                        if ui.button("📄 Nový soubor   Ctrl+N").clicked() { ctx_action = Some(ContextAction::NewFile); ui.close_menu(); }
                        ui.separator();
                        if ui.button("☑ Označit vše").clicked()  { ctx_action = Some(ContextAction::SelectAll);   ui.close_menu(); }
                        if ui.button("☐ Odznačit vše").clicked() { ctx_action = Some(ContextAction::DeselectAll); ui.close_menu(); }
                    });
                }

                if go_up       { *active = which; panel.go_up(); }
                if let Some(n) = enter_target { *active = which; panel.enter(&n); }
                if let Some(p) = open_zip     { *active = which; panel.enter_zip_archive(p); }

                // Drop target - zvýraznění panelu při přetahování a spuštění přesunu při puštění
                if drag_src.is_some() && drag_src != &Some(which) {
                    // Zvýraznění drop zóny
                    let panel_rect = ui.min_rect();
                    ui.painter().rect_stroke(panel_rect, 4.0,
                        egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(80, 200, 120)));

                    // Puštění myši = drop → přesun
                    if ui.input(|i| i.pointer.any_released()) {
                        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                            if panel_rect.contains(pos) {
                                ctx_action = Some(ContextAction::Move);
                                *drag_src = None;
                            }
                        }
                    }
                }

                // Konec tažení bez dropu - reset
                if ui.input(|i| i.pointer.any_released()) && drag_src == &Some(which) {
                    *drag_src = None;
                }

            });
    });

    (panel_action, ctx_action)
}

/// Načte uloženou pozici a velikost okna
fn load_window_state() -> Option<(i32, i32, i32, i32, bool)> {
    let path = dirs_home().join(".er_commander_window.txt");
    let content = fs::read_to_string(path).ok()?;
    let mut x = 100i32; let mut y = 100i32;
    let mut w = 1300i32; let mut h = 700i32;
    let mut maximized = false;
    let mut has_size = false;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("WIN_X=") { x = v.trim().parse().unwrap_or(100); }
        else if let Some(v) = line.strip_prefix("WIN_Y=") { y = v.trim().parse().unwrap_or(100); }
        else if let Some(v) = line.strip_prefix("WIN_W=") { w = v.trim().parse().unwrap_or(1300); has_size = true; }
        else if let Some(v) = line.strip_prefix("WIN_H=") { h = v.trim().parse().unwrap_or(700); }
        else if let Some(v) = line.strip_prefix("WIN_MAX=") { maximized = v.trim() == "true"; }
        // WIN_MIN ignorujeme - minimalizované okno otevřeme normálně
    }
    Some((x, y, if has_size { w } else { 1300 }, if has_size { h } else { 700 }, maximized))
}

/// Na Linuxu appka nemá instalátor - je to jeden přenosný soubor. Aby se
/// ale i tak objevila v menu aplikací (Nástroje/Systém apod.), zapíše si
/// při každém spuštění vlastní .desktop záznam a ikonu do standardních
/// XDG umístění pod $HOME. Ukazuje vždy na AKTUÁLNÍ cestu binárky (podle
/// `current_exe()`), takže i po přesunutí souboru se zástupce při dalším
/// spuštění sám opraví. Vše je best-effort - chyby (např. bez domovského
/// adresáře) se tiše ignorují, appka kvůli tomu nesmí spadnout.
#[cfg(target_os = "linux")]
fn ensure_linux_desktop_entry() {
    use std::io::Write;

    let Ok(exe_path) = std::env::current_exe() else { return };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };

    let icon_dir = home.join(".local/share/icons/hicolor/256x256/apps");
    let apps_dir = home.join(".local/share/applications");
    if fs::create_dir_all(&icon_dir).is_err() || fs::create_dir_all(&apps_dir).is_err() {
        return;
    }

    let icon_path = icon_dir.join("er-commander.png");
    if let Ok(mut f) = File::create(&icon_path) {
        let _ = f.write_all(include_bytes!("../assets/app_icon.png"));
    }

    let desktop_path = apps_dir.join("er-commander.desktop");
    let content = format!(
        "[Desktop Entry]\nType=Application\nName=eR Commander\nComment=Dvoupanelový správce souborů\nExec=\"{}\"\nIcon=er-commander\nCategories=Utility;FileManager;System;\nTerminal=false\nStartupNotify=true\nStartupWMClass=eR_Commander\n",
        exe_path.display()
    );
    if fs::write(&desktop_path, content).is_err() {
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&desktop_path) {
            let mut perm = meta.permissions();
            perm.set_mode(0o755);
            let _ = fs::set_permissions(&desktop_path, perm);
        }
    }

    // Ať se menu (GNOME/KDE/XFCE...) obnoví hned, pokud nástroj existuje.
    // Když chybí, jednoduše se přeskočí - většina prostředí si všimne i tak.
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&apps_dir)
        .status();
}

fn main() -> eframe::Result<()> {
    #[cfg(target_os = "linux")]
    ensure_linux_desktop_entry();

    let win = load_window_state().unwrap_or((100, 100, 1300, 700, false));
    let (win_x, win_y, win_w, win_h, win_max) = win;

    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([win_w as f32, win_h as f32])
        .with_position(egui::pos2(win_x as f32, win_y as f32))
        .with_maximized(win_max)
        .with_app_id("eR_Commander")
        .with_icon(
            eframe::icon_data::from_png_bytes(
                include_bytes!("../assets/app_icon.png")
            ).unwrap_or_default()
        );

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "eR Commander",
        options,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);

            // Přidáme emoji font - na Windows je Segoe UI Emoji vždy dostupný
            // a obsahuje všechny emoji které používáme (📁📄📦⬆★).
            // Nastavíme ho jako fallback font, aby se použil pro znaky
            // které základní egui font nezná.
            let mut fonts = egui::FontDefinitions::default();

            #[cfg(windows)]
            {
                // Segoe UI Emoji - systémový font Windows s plnou emoji podporou
                let emoji_font_path = "C:\\Windows\\Fonts\\seguiemj.ttf";
                if let Ok(font_data) = std::fs::read(emoji_font_path) {
                    fonts.font_data.insert(
                        "segoe_emoji".to_owned(),
                        egui::FontData::from_owned(font_data),
                    );
                    // Přidáme jako poslední fallback pro proportional text
                    fonts.families
                        .get_mut(&egui::FontFamily::Proportional)
                        .unwrap()
                        .push("segoe_emoji".to_owned());
                    fonts.families
                        .get_mut(&egui::FontFamily::Monospace)
                        .unwrap()
                        .push("segoe_emoji".to_owned());
                }
            }

            cc.egui_ctx.set_fonts(fonts);

            // Vypneme selectable_labels aby widgety nebraly focus kliknutím
            cc.egui_ctx.style_mut(|s| {
                s.interaction.selectable_labels = false;
            });

            let mut app = FileManagerApp::default();
            app.maximize_on_start = win_max;
            Ok(Box::new(app))
        }),
    )
}
