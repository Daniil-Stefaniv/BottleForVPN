// Тесты контракта CLI↔GUI (M4 Task 3): разбор CliReply из stdout
// подпроцесса bottle — ровно та форма, что запинена golden-тестами
// Task 1 (внешний тег CamelCase, поля snake_case).

import { describe, expect, it } from "vitest";
import { isDaemonUnavailable, parseCliReply } from "./ipc";

describe("parseCliReply", () => {
  it("разбирает List из golden-формы Task 1", () => {
    const reply = parseCliReply({
      stdout:
        '{"List":{"bottles":[{"id":"office-a","name":"Офис А","kind":"env","state":"up"},{"id":"corp","name":"Корп VPN","kind":"host","state":null}]}}',
      code: 0,
    });
    expect(reply).toEqual({
      List: {
        bottles: [
          { id: "office-a", name: "Офис А", kind: "env", state: "up" },
          { id: "corp", name: "Корп VPN", kind: "host", state: null },
        ],
      },
    });
  });

  it("разбирает Status с Degraded-объектом состояния", () => {
    const reply = parseCliReply({
      stdout:
        '{"Status":{"bottles":[{"id":"office-b","state":{"Degraded":{"reason":"агент аплаенса не отвечает"}},"socks5":null,"forwards":[]}]}}',
      code: 0,
    });
    if (!("Status" in reply)) throw new Error("ожидался Status");
    expect(reply.Status.bottles[0]?.state).toEqual({
      Degraded: { reason: "агент аплаенса не отвечает" },
    });
  });

  it("Error-ответ — исключение с сообщением демона дословно", () => {
    expect(() =>
      parseCliReply({
        stdout: '{"Error":{"message":"бутылка office-a не найдена"}}',
        code: 1,
      }),
    ).toThrowError("бутылка office-a не найдена");
  });

  it("не-JSON stdout (например, код 2) — понятная ошибка с фрагментом вывода", () => {
    expect(() =>
      parseCliReply({ stdout: "error: unexpected argument", code: 2 }),
    ).toThrowError(/не-JSON ответ \(код 2\).*unexpected argument/);
  });
});

describe("isDaemonUnavailable", () => {
  it("узнаёт ошибку «запущен ли pogrebok-daemon» по тексту CLI", () => {
    expect(
      isDaemonUnavailable(
        new Error(
          "Не удалось подключиться к демону Погребка — запущен ли pogrebok-daemon?",
        ),
      ),
    ).toBe(true);
  });

  it("прочие ошибки — не «демон недоступен»", () => {
    expect(isDaemonUnavailable(new Error("бутылка не найдена"))).toBe(false);
    expect(isDaemonUnavailable("не ошибка-объект")).toBe(false);
  });
});
