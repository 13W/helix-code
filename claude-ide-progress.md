# Прогресс: Claude Code IDE-интеграция (по `claude-ide-tasks.md`)

Протокол: `claude-code-ide-protocol-spec.md` (CLI 2.1.261, расширение 2.1.261). Базовый коммит: `4a5447714`.

| Задача | Статус | Коммит | Заметки |
|---|---|---|---|
| T0 · Гигиена зависимостей | готово | `be5ec90c2` | `reqwest` 0.13, `dashmap` 6.2, `similar` 3, патчи lock; `cargo tree -e normal -p helix-term \| grep reqwest` пуст |
| T1 · Крейт `helix-claude-ide` | готово | `a468c9848` | транспорт, lock-файл, JSON-RPC, `initialize`/`ping`/`tools/list`/`tools/call` с заглушкой; 20 юнит + 7 интеграционных тестов (`tests/handshake.rs`); пример `examples/serve.rs`. **Живая проверка с `claude` 2.1.261**: `claude --ide` в каталоге workspace → `initialize` (2025-11-25) → `notifications/initialized` → `ide_connected {pid}` → `tools/list`; `/exit` → close 1000; Ctrl-C серверу удаляет lock. `claude -p` (print-режим) к IDE **не** подключается — проверять только интерактивно. |
| T2 · Интеграция в helix-term | готово | `74ad25cc2` | флаги `--claude-ide`, `--claude-ide-port`, `--claude-ide-name`; конфиг `[editor.claude-ide]` (`enable`, `name`, `notify-selection`, `diff-mode`); `Editor.claude_ide: Option<Session>`; `:claude-ide-start/stop/status`; statusline `claude-ide-indicator` (`✻` / `✻◆`); lock удаляется в `Application::close()`, panic-хуке и перед `process::exit` (SIGTSTP); `EditorHandler` с `mcp_tx` (инструменты — заглушки до T3/T5). Тест `helix-term/tests/claude_ide_integration.rs`. **Живая проверка**: `hx --claude-ide` в pty + `claude --ide` → `:claude-ide-status` показал `client connected`; `:q!` удалил lock. Документация: `book/src/editor.md` (секция и элемент statusline), `typable-cmd.md` перегенерирован. |
| T3 · `getDiagnostics` | готово | (следующий за T2 коммит) | Источник — `editor.diagnostics` (LSP-диагностики всех файлов, не только открытых) + открытые документы для `linesInFile`; `helix-mcp-types`: `DiagnosticItem` получил `end_line/end_col`, `col` теперь LSP-character (раньше был char-offset документа — баг), ответ `GetDiagnostics` сгруппирован в `Vec<FileDiagnostics>` (потребитель `helix-mcp` lsp_extras обновлён). `helix-claude-ide/src/diagnostics.rs`: `uri↔path`, формат VS Code (`Error/Warning/Information/Hint`, `range.start/end.line/character`, `source`, `code`, `linesInFile`), `to_string_pretty`. Пример-клиент `examples/client.rs`. **Живая проверка**: clangd на `main.c` с `undefined_symbol` → точный формат; реальный `claude` при Edit: baseline `getDiagnostics {uri}` 16 мс, после правки `getDiagnostics {}` 8 мс, без path_mismatch. |
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

## Наблюдения по реальному CLI 2.1.261 (уточнения к PROTO)

- **CLI переподключается сам** после разрыва WebSocket (`WebSocket transport closed/disconnected, attempting automatic reconnection`, попытки с backoff 1 с, 2 с, …) — PROTO §2.6 утверждает обратное. Для Helix это значит, что после `:claude-ide-stop`/`:claude-ide-start` на том же порту CLI вернётся сам.
- CLI вызывает инструмент **`set_permission_mode {mode}`** при смене режима разрешений (`callIdeRpc("set_permission_mode", …)`, ошибки глотает) — в PROTO не описан; Helix отвечает `-32602 Tool not found`, это безвредно. Кандидат на no-op-реализацию в T7.
- Ошибочный результат `isError:true` от `closeAllDiffTabs` CLI пишет в debug-лог как `[ERROR]`, но продолжает работу (до T5 инструмент — заглушка).
- Диагностики отражают **буфер Helix**: после правки файла CLI-инструментом Edit открытый в Helix документ не перечитывается, LSP видит старое содержимое, поэтому «новые диагностики» появятся только после `:reload`. В T5 после `FILE_SAVED` стоит перечитывать документ автоматически (файл на диск пишет CLI).
- `claude -p` (print-режим) к IDE не подключается; при вводе через pty текст и `\r` нужно слать раздельно, иначе Enter трактуется как часть вставки и промпт не отправляется.

## Issues (вне плана)

- `helix-term/tests/mcp_integration.rs::test_editor_tools` падает и на базовом коммите `4a5447714`: тест шлёт `edit_file {edits:[{start_line,end_line,new_text}]}`, а инструмент (`helix-mcp/src/tools/write.rs:137`) ждёт `old_string/new_string`. Ошибка: `-32602 missing field new_string`. Не связано с T0; 15 остальных тестов зелёные.

- `MCP_AUTO_APPROVE` (`helix-mcp/src/channel.rs:85`) по умолчанию `true` — все write-операции MCP применяются без подтверждения даже без `--mcp-auto-approve`. Отдельная задача безопасности, здесь не трогается.
