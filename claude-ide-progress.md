# Прогресс: Claude Code IDE-интеграция (по `claude-ide-tasks.md`)

Протокол: `claude-code-ide-protocol-spec.md` (CLI 2.1.261, расширение 2.1.261). Базовый коммит: `4a5447714`.

| Задача | Статус | Коммит | Заметки |
|---|---|---|---|
| T0 · Гигиена зависимостей | готово | `be5ec90c2` | `reqwest` 0.13, `dashmap` 6.2, `similar` 3, патчи lock; `cargo tree -e normal -p helix-term \| grep reqwest` пуст |
| T1 · Крейт `helix-claude-ide` | готово | `3de3b5b9d` | транспорт, lock-файл, JSON-RPC, `initialize`/`ping`/`tools/list`/`tools/call` с заглушкой; 20 юнит + 7 интеграционных тестов (`tests/handshake.rs`); пример `examples/serve.rs`. **Живая проверка с `claude` 2.1.261**: `claude --ide` в каталоге workspace → `initialize` (2025-11-25) → `notifications/initialized` → `ide_connected {pid}` → `tools/list`; `/exit` → close 1000; Ctrl-C серверу удаляет lock. `claude -p` (print-режим) к IDE **не** подключается — проверять только интерактивно. |
| T2 · Интеграция в helix-term | — | — | |
| T3 · `getDiagnostics` | — | — | |
| T4 · `selection_changed` / `at_mentioned` | — | — | |
| T5 · `openDiff` (prompt), `close_tab`, `closeAllDiffTabs` | — | — | |
| T6a · Diff-вью: отрисовка | — | — | |
| T6b · Diff-вью: принятие/отказ | — | — | |
| T7 · Приёмка, регресс, документация | — | — | |

## Отклонения от tasks.md

- Документ FORK (`claude-ide-spec.md`) в репозитории отсутствует; все ссылки на него заменены проверкой по коду.
- T0: `reqwest` уже был в `[dev-dependencies]` helix-term (0.12) — только поднят до 0.13; `dashmap` в lock уже 6.2.1.
- T0: `similar` в `helix-mcp/Cargo.toml` был объявлен, но не использовался — удалён вместо апгрейда.
- Конфиг размещается в `[editor.claude-ide]`, а не top-level `[claude-ide]`: значения (`notify-selection`, `diff-mode`) нужны в рантайме из `Editor`/`Document::config`, top-level секция туда не доходит (см. T2).

## Issues (вне плана)

- `helix-term/tests/mcp_integration.rs::test_editor_tools` падает и на базовом коммите `4a5447714`: тест шлёт `edit_file {edits:[{start_line,end_line,new_text}]}`, а инструмент (`helix-mcp/src/tools/write.rs:137`) ждёт `old_string/new_string`. Ошибка: `-32602 missing field new_string`. Не связано с T0; 15 остальных тестов зелёные.

- `MCP_AUTO_APPROVE` (`helix-mcp/src/channel.rs:85`) по умолчанию `true` — все write-операции MCP применяются без подтверждения даже без `--mcp-auto-approve`. Отдельная задача безопасности, здесь не трогается.
