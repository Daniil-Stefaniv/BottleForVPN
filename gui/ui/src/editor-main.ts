// Точка входа окна редактора (editor.html) внутри Tauri: GuiApi поверх
// invoke, контекст — из Rust (open_editor положил id/состояние), выбор
// файла — системный диалог, закрытие — close() окна (родитель
// разблокируется событием Destroyed в Rust).

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { setGuiApi } from "./backend";
import { mountEditor, setEditorHost } from "./editor";
import type { EditorContextWire } from "./ipc";
import { makeTauriGuiApi } from "./tauri-api";

setGuiApi(makeTauriGuiApi());

setEditorHost({
  context: () => invoke<EditorContextWire>("editor_context"),
  pickConfig: async () => {
    const selected = await open({
      multiple: false,
      directory: false,
      title: "Файл конфигурации туннеля",
    });
    return typeof selected === "string" ? selected : null;
  },
  close: () => {
    void getCurrentWindow().close();
  },
});

const root = document.getElementById("editor-root");
if (root !== null) {
  void mountEditor(root);
}
