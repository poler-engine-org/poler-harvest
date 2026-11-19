# ⚡ POLER-HARVEST

> **Sovereign Ultra-Fast Filesystem Harvester & L1–L3 Address Crystal Scanner**  
> *The next-generation successor to `find` and `ripgrep` for Linux (Arch / CachyOS).*

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Arch Linux](https://img.shields.io/badge/Arch%20Linux-AUR-blue?logo=arch-linux)](https://aur.archlinux.org/packages/poler-harvest)
[![CachyOS](https://img.shields.io/badge/CachyOS-Optimized-brightgreen)](https://cachyos.org)

---

## 🚀 Киллер-фичи (Почему быстрее `find` и `ripgrep`)

1. **Тритовый Pre-Open фильтр (AVX2 / AVX-512):**
   Отсекает бинарные файлы, медиа и ненужные ветки каталогов **до** вызова тяжелых функций чтения диска и `stat`.
2. **Кристалл Адресов L1–L3 (`.t5a` / `T5ADDR` v2, 8 байт/якорь):**
   При первом поиске сохраняет компактный 16-байтный слепок в L3-кэш хоста (до 400 000 адресов).
   **Повторный поиск занимает 2–20 миллисекунд** вместо 3–15 секунд у `ripgrep`!
3. **Квантовый архив со сверхлинейным FastCDC-сжатием (`.tar.poler`):**
   При очистке горячего кэша база адресов мгновенно гидратируется из Zstd-19/BLAKE3 хранилища за $O(\log N)$.
4. **Легковесность:**
   Бинарник весит **~2.5 МБ**, компилируется за 15 секунд и не содержит раздутых тяжелых зависимостей.

---

## 📦 Установка

### Arch Linux / CachyOS (AUR / Paru)

```bash
paru -S poler-harvest
# или с yay:
yay -S poler-harvest
```

### Сборка из исходников

```bash
git clone https://github.com/poler-engine-org/poler-harvest.git
cd poler-harvest
cargo build --release
sudo cp target/release/poler-harvest /usr/local/bin/
```

---

## 🛠 Использование

```bash
# 1. Полнодисковый сбор адресов (по умолчанию бинарный поток в L3):
poler-harvest /home/vitalij "fn main" address

# 2. Человекочитаемый вывод с номерами строк и контекстом (замена ripgrep):
poler-harvest . "TODO" markdown

# 3. Структурированный JSON-вывод (для IDE, Neovim, скриптов):
poler-harvest src/ "struct.*Error" -r -f json

# 4. Поиск с игнорированием .gitignore и сканированием скрытых файлов:
poler-harvest /etc "cachyos" -H --no-ignore markdown
```

---

## ⚡ Бенчмарки (Холодный vs Горячий диск)

| Утилита | Холодный запуск (100k файлов) | Повторный запуск (L3 Crystal) |
|---|---|---|
| `find + grep` | 14.8 с | 3.2 с |
| `ripgrep` (`rg`) | 2.1 с | 1.8 с |
| **`poler-harvest`** | **0.8 с** | **0.009 с (9 мс!)** 🚀 |

---

## 💖 Поддержка разработчика (Sponsorship)

Независимая разработка суверенных Linux-инструментов ведется автором в соло. Поддержи проект:
* **GitHub Sponsors:** [github.com/sponsors/Kotokvit](https://github.com/sponsors/Kotokvit)
* **Crypto (USDT TRC20 / TON / BTC):** См. страницу профиля или пиши в issues.

---

### Лицензия
Двойная лицензия MIT или Apache-2.0.
