# ⚡ POLER-HARVEST

> **Content-Addressed Filesystem Search with Persistent L1–L3 Cache**  
> *Instant 2 ms warm lookups for monorepos, developer workspaces, and AI agents.*

[![CI](https://github.com/poler-engine-org/poler-harvest/actions/workflows/ci.yml/badge.svg)](https://github.com/poler-engine-org/poler-harvest/actions)
[![License: POLER Source-Available 1.0](https://img.shields.io/badge/License-POLER--1.0%20Source--Available-blue.svg)](LICENSE)
[![Arch Linux](https://img.shields.io/badge/Arch%20Linux-AUR-blue?logo=arch-linux)](https://aur.archlinux.org/packages/poler-harvest)
[![CachyOS](https://img.shields.io/badge/CachyOS-Optimized-brightgreen)](https://cachyos.org)

---

## 🎯 Что это и для чего создано?

`grep` и `ripgrep` превосходны для одноразового поиска, но у них нет памяти: каждый запуск они заново сканируют всё дерево каталогов с диска.

В больших репозиториях, CI-пайплайнах и сценариях работы **AI-агентов** (где агент делает десятки поисковых запросов в одном и том же дереве файлов), повторное чтение миллионов строк — это пустая трата ресурсов и секунд ожидания.

**`poler-harvest` решает эту задачу:**
1. **Холодный запуск (Cold Run):** сканирует файлы параллельно (Aho-Corasick / SIMD / Regex), выдаёт результаты и автоматически индексирует карту совпадений в компактный кэш (`.t5a` / `.tar.poler`).
2. **Горячий запуск (Warm Run):** возвращает результаты за **2 миллисекунды** — в 100–400 раз быстрее ripgrep!
3. **Честная контентная инвалидация (7/7 проверок):** кэш автоматически отслеживает изменение, добавление или удаление файлов по `size`, `mtime` и BLAKE3-хешу, точечно пересканируя только то, что изменилось. Никаких устаревших результатов.

---

## 📊 Честные бенчмарки

> Замеры на независимом стенде (Debian 12/13, x86_64, Linux 6.x).

### 1. Синтетический репозиторий: 100 000 файлов (12 МБ)
| Инструмент | Холодный поиск | Повторный поиск (Warm Cache) |
|---|---|---|
| `GNU grep 3.8` (`grep -rInF`) | 407–451 мс | 407–451 мс *(кэша нет)* |
| `ripgrep 13.0` (`rg -F`) | 728–820 мс | 728–820 мс *(кэша нет)* |
| **`poler-harvest`** | ~2.5 с *(плата за запись кэша)* | **2 мс (в 200–400× быстрее!)** ⚡ |
| **`poler-harvest --no-cache`** | **~650 мс** *(прямой параллельный скан)* | — |

### 2. Реальный корпус: Python 3.11 Stdlib (1 403 файла, 54 МБ)
| Инструмент | Холодный поиск | Повторный поиск |
|---|---|---|
| `ripgrep 13.0` | 12 мс | 12 мс |
| `GNU grep 3.8` | 19–90 мс | 19–90 мс |
| **`poler-harvest`** | 215 мс *(холодный с кэшированием)* | **2 мс (в 6–10× быстрее rg!)** ⚡ |

> 💡 **Резюме:** если нужен один разовый поиск — используйте `rg` или `poler-harvest --no-cache`. Если вы работаете в проекте постоянно или ваш AI-агент ищет по кодовой базе — `poler-harvest` экономит 99% времени на каждом запросе.

---

## 🚀 Форматы и возможности

- **`address` (по умолчанию):** бинарный/JSONL поток точных якорей `(path, offset, len, matches)` для IDE, LSP и AI-агентов. Zero-copy интеграция без перепарсинга файлов.
- **`markdown`:** чистый человекочитаемый вывод с номерами строк и контекстом (`-c <N>`).
- **`json`:** построчный JSONL для интеграции в тулчейн и скрипты.
- **`corpus`:** чистый контент совпадений для промптов больших языковых моделей (LLM).
- **Поиск фраз целиком:** регистронезависимый поиск точных фраз с полной поддержкой Unicode Case-Folding (кириллица, европейские языки).
- **Регулярные выражения (`-r`):** полноценный regex-движок.
- **Управление игнорированием:** уважает `.gitignore` и скрытые файлы по умолчанию; `-H` включает скрытые, `--no-ignore` отключает все игноры (включая `target/` и `node_modules/`).
- **Флаг `--no-cache`:** прямой поиск без чтения и сохранения кэша на диск.
- **Семантическая подсветка (`--color <auto|always|never>`):** каждый элемент имеет свой цвет в интерактивном терминале (путь — cyan, адрес — green, разделитель — gray, запрос — bold yellow, вывод — default). В пайпах и для ИИ-агентов подсветка автоматически отключается (`auto`), выдавая 100% чистый текст без ANSI-кодов.

---

## 📦 Установка

### 1. Универсальный 1-строчник (работает на любом Linux, без root):
```bash
curl -sSL https://raw.githubusercontent.com/poler-engine-org/poler-harvest/main/scripts/install.sh | bash
```

### 2. Для AI-агентов и скриптов (Python / Pip):
```bash
# В виртуальном окружении (venv / conda):
pip install https://github.com/poler-engine-org/poler-harvest/releases/download/v0.96.1/poler_harvest-0.96.1-py3-none-manylinux_2_34_x86_64.whl

# В песочницах без venv (Debian 12+, Ubuntu 23.04+ с PEP 668):
pip install --break-system-packages https://github.com/poler-engine-org/poler-harvest/releases/download/v0.96.1/poler_harvest-0.96.1-py3-none-manylinux_2_34_x86_64.whl
```

#### Использование прямо в Python:
```python
import poler_harvest

# Быстрый поиск файлов (возвращает markdown, json или corpus):
output = poler_harvest.search(root=".", query="TODO: fix", format="markdown")
print(output)
```

### 3. Arch Linux / CachyOS (Pacman / Paru):
```bash
curl -LO https://github.com/poler-engine-org/poler-harvest/releases/download/v0.96.1/poler-harvest-0.96.1-1-x86_64.pkg.tar.zst
sudo pacman -U poler-harvest-0.96.1-1-x86_64.pkg.tar.zst
```

### 4. Debian / Ubuntu / Mint (.deb):
```bash
curl -LO https://github.com/poler-engine-org/poler-harvest/releases/download/v0.96.1/poler-harvest_0.96.1_amd64.deb
sudo dpkg -i poler-harvest_0.96.1_amd64.deb
```

### 5. Conda / Anaconda:
```bash
conda install https://github.com/poler-engine-org/poler-harvest/releases/download/v0.96.1/poler-harvest-0.96.1-linux_64.tar.bz2
```

---

## 🛠 Примеры команд

```bash
# Точный поиск фразы с выводом якорей (2 мс при повторе):
poler-harvest . "TODO: fix this bug" address

# Поиск по коду на русском/украинском языке с контекстом:
poler-harvest src/ "Ошибка сети" markdown -c 2

# Регулярное выражение с JSONL-выводом:
poler-harvest src/ "struct.*Error" -r -f json

# Поиск везде, включая target/ и скрытые файлы:
poler-harvest . "secret_token" -H --no-ignore

# Разовый прямой поиск без кэширования:
poler-harvest /var/log "kernel error" --no-cache
```

---

## 💖 Поддержка проекта / Sponsorship

Если инструмент сэкономил вам время и ресурсы, вы можете поддержать независимого разработчика:

* **USDT (TRC-20):** `THLxXNLxxoYA5rgyinypexiaD3Nb9iXo7a`  
  *(Сеть TRON, QR-код и баланс в [Tronscan](https://tronscan.org/#/address/THLxXNLxxoYA5rgyinypexiaD3Nb9iXo7a))*
* **Binance Pay ID:** `892674734` *(User: User-770b3 — мгновенный донат без комиссии со всего мира)*
* **Email для связи / B2B:** `vitalijkotok18@gmail.com`
* **Телефон / Telegram:** `+380937981708`

---

## ⚖️ Лицензия и условия использования / License

Проект распространяется под лицензией **POLER Custom Source-Available & Modification Disclosure License Version 1.0** (см. полный текст [`LICENSE`](LICENSE) и практическое руководство [`TERMS.md`](TERMS.md)).

* 🔍 **Открытый исходный код (Source-Available):** Полная свобода изучения, аудита безопасности, локальной компиляции, бенчмаркинга и персонального использования.
* 🛡️ **Защита от закрытых форков (Anti-Silent-Forking):** Включение ядра или модификаций в сторонние коммерческие продукты и платформы требует обязательного уведомления правообладателя (`vitalijkotok18@gmail.com` или через Issue в репозитории) в течение 14 дней с момента первого релиза/деплоя.
* 💎 **Бесплатно для сообщества:** Личное использование, независимые исследования и интеграции внутри открытых инструментов — абсолютно бесплатны. Роялти 5% действует исключительно при валовой выручке коммерческого продукта свыше **$25 000 в квартал** (с безопасной гаванью до $10 000).
