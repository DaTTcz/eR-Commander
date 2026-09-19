<p align="center">
  <img src="app_icon.png" alt="eR Commander logo" width="200">
</p>
<p align="center">
  <a href="https://www.paypal.com/paypalme/DaTTcz">
    <img src="https://img.shields.io/badge/%E2%9D%A4%EF%B8%8F_Podpo%C5%99_projekt-PayPal-ffc439?style=for-the-badge&logo=paypal&logoColor=ffc439&labelColor=003087" alt="Podpořit přes PayPal">
  </a>
  &nbsp;&nbsp;
  <a href="https://ko-fi.com/dattcz">
    <img src="https://img.shields.io/badge/%E2%98%95_Ko--fi-dattcz-ff5e5b?style=for-the-badge&logo=ko-fi&logoColor=white" alt="Podpořit na Ko-fi">
  </a>
</p>

# eR Commander

**Dvoupanelový správce souborů pro Windows** postavený v [Rustu](https://www.rust-lang.org/) s použitím GUI frameworku [egui](https://github.com/emilk/egui).

Inspirován klasickými správci souborů jako Total Commander – rychlý, přehledný, bez zbytečností.

---

## Funkce

### Panely a navigace
- Dva panely vedle sebe, přepínání klávesou **Tab**
- Klávesová navigace (šipky, PgUp/PgDn, Home/End)
- Breadcrumb navigace kliknutím na části cesty
- Adresní řádek s ručním zadáním cesty (Enter = přejít)
- Přejmenování složek a souborů přímo v řádku (**F2**)
- Záložky oblíbených složek s podporou dvou panelů najednou

### Souborové operace
- **F5** Kopírovat – potvrzovací dialog se zdrojem a cílem, možnost přejmenovat při kopírování
- **F6** Přesunout – s progress barem
- **F7** Nová složka
- **F8** / **Del** Smazat s potvrzením
- **Ctrl+N** Nový textový soubor
- Drag & drop přesun mezi panely
- Zpracování chyb za běhu – při chybě dialog Opakovat / Přeskočit / Zrušit

### Výběr souborů
- **Mezerník** označí / odznačí soubor
- **+** / **-** výběr pomocí glob masky (např. `*.mkv`)
- Statistiky výběru v dolní liště (počet, velikost včetně složek)
- Rekurzivní výpočet velikosti složek na pozadí

### Zobrazení
- Sloupce: Název, Přípona, Velikost, Datum, Attr – řaditelné kliknutím
- Emoji ikonky (📁📄📦)
- Světlé / tmavé barevné téma
- Nastavení řazení složek (vždy nahoře / smíchány)

### Editory
- **F3** Vestavěný textový editor (monospace, Ctrl+S uložení)
- **F4** Externí editor – nastavitelná cesta k exe (Notepad++, VS Code, …)

### Archívy
- ZIP archívy jako průchozí složky (procházení, rozbalování)
- RAR / 7z otevírá systémová aplikace

### Hledání
- **Alt+F7** Hledání souborů podle jména a/nebo obsahu textu

### Ostatní
- **Auto-update** z GitHubu – při startu tiše zkontroluje novou verzi a nabídne stažení
- Paměť: cesty, řazení, záložky, téma, velikost a pozice okna
- Kontextové menu pravým tlačítkem myši

---

## Klávesové zkratky

| Klávesa | Akce |
|---------|------|
| F2 | Přejmenovat |
| F3 | Textový editor |
| F4 | Externí editor |
| F5 | Kopírovat |
| F6 | Přesunout |
| F7 | Nová složka |
| F8 / Del | Smazat |
| Alt+F7 | Hledat soubory/text |
| Ctrl+M | Hromadné přejmenování |
| Ctrl+N | Nový textový soubor |
| Tab | Přepnout panel |
| Mezerník | Označit / odznačit |
| + / - | Označit / odznačit maskou |

---

## Instalace

Stáhni nejnovější `eR_Commander.exe` ze záložky [Releases](https://github.com/DaTTcz/eR-Commander/releases) a spusť – žádná instalace není potřeba, vše je v jednom souboru.

---

## Sestavení ze zdrojového kódu

### Požadavky
- [Rust](https://rustup.rs/) (stable toolchain)
- Windows (primárně testováno na Windows 10/11)

### Postup

```powershell
git clone https://github.com/DaTTcz/eR-Commander.git
cd eR-Commander
cargo build --release
```

Výsledný exe bude v `target\release\`.

---

## Technologie

- **Rust** – systémový jazyk, rychlost a bezpečnost
- **egui / eframe** – okamžité GUI, bez závislostí na OS widgetech
- **rayon** – paralelní souborové operace
- **notify** – sledování změn v souborovém systému (auto-refresh)
- **zip** – práce s ZIP archivy
- **reqwest** – kontrola aktualizací z GitHubu

---

## Autor

**David Trubka** – [DaTT.cz](https://datt.cz)

---

## Licence

[PolyForm Noncommercial License 1.0.0](LICENSE)

Volné použití pro nekomerční účely. Komerční využití vyžaduje samostatnou dohodu s autorem.
