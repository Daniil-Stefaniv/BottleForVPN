// Тесты чистых функций сетки бутылок (0.3): merge профилей БД с
// рантайм-событиями по id, подписи состояний, мета-строки, блокировка
// кнопок в переходах. DOM-рендер сетки — grid.test.ts.

import { describe, expect, it } from "vitest";
import {
  bottleActionsLocked,
  mergeListStatus,
  rowMeta,
  stateLabel,
  statePillClass,
  type BottleRow,
} from "./list";
import type { BottleListRowWire, RuntimeState } from "./ipc";

const summary = (over: Partial<BottleListRowWire>): BottleListRowWire => ({
  id: "office-a",
  name: "Офис А",
  kind: "env",
  protocol: "wireguard",
  has_credentials: false,
  state: null,
  ...over,
});

const runtime = (over: Partial<RuntimeState>): RuntimeState => ({
  id: "office-a",
  state: "Up",
  socks5: 1080,
  forwards: [],
  ...over,
});

const row = (over: Partial<BottleRow>): BottleRow => ({
  id: "office-a",
  name: "Офис А",
  kind: "env",
  protocol: "wireguard",
  hasCredentials: false,
  state: "up",
  socks5: 1080,
  forwards: [],
  ...over,
});

describe("mergeListStatus", () => {
  it("объединяет БД и рантайм по id: рантайм важнее, socks5/forwards из него", () => {
    const rows = mergeListStatus(
      [summary({ id: "a", state: null }), summary({ id: "b" })],
      [
        runtime({ id: "a", state: "Starting", socks5: 55001 }),
        runtime({ id: "ghost", socks5: 999 }),
      ],
    );
    expect(rows).toEqual([
      {
        id: "a",
        name: "Офис А",
        kind: "env",
        protocol: "wireguard",
        hasCredentials: false,
        state: "starting",
        reason: undefined,
        socks5: 55001,
        forwards: [],
      },
      {
        id: "b",
        name: "Офис А",
        kind: "env",
        protocol: "wireguard",
        hasCredentials: false,
        state: "stopped",
        reason: undefined,
        socks5: null,
        forwards: [],
      },
      {
        id: "ghost",
        name: "",
        kind: "env",
        protocol: null,
        hasCredentials: false,
        state: "up",
        reason: undefined,
        socks5: 999,
        forwards: [],
      },
    ]);
  });

  it("runtime без профиля тоже показывается (сирота)", () => {
    const rows = mergeListStatus([], [runtime({ id: "ghost", state: "Up" })]);
    expect(rows).toHaveLength(1);
    expect(rows[0]?.id).toBe("ghost");
    expect(rows[0]?.name).toBe("");
  });

  it("Degraded/Failed-объекты состояния нормализуются в канонические имена", () => {
    const rows = mergeListStatus(
      [summary({ id: "d" }), summary({ id: "f" })],
      [
        runtime({ id: "d", state: { Degraded: { reason: "агент молчит" } } }),
        runtime({ id: "f", state: { Failed: { reason: "туннель не поднялся" } } }),
      ],
    );
    expect(rows[0]?.state).toBe("degraded");
    expect(rows[0]?.reason).toBe("агент молчит");
    expect(rows[1]?.state).toBe("failed");
  });

  it("протокол и флаг кредов переносятся из строки БД", () => {
    const rows = mergeListStatus(
      [summary({ protocol: "openvpn", has_credentials: true })],
      [],
    );
    expect(rows[0]?.protocol).toBe("openvpn");
    expect(rows[0]?.hasCredentials).toBe(true);
  });

  it("пустые входы — пустой список", () => {
    expect(mergeListStatus([], [])).toEqual([]);
  });
});

describe("stateLabel", () => {
  it("русские подписи состояний с причиной деградации/отказа", () => {
    expect(stateLabel("up")).toBe("работает");
    expect(stateLabel("stopped")).toBe("остановлена");
    expect(stateLabel("degraded", "агент молчит")).toBe("деградация: агент молчит");
    expect(stateLabel("failed", "туннель не поднялся")).toBe(
      "ошибка: туннель не поднялся",
    );
  });
});

describe("statePillClass", () => {
  it("пилюли: up зелёная, переходы и деградация — свои классы", () => {
    expect(statePillClass("up")).toBe("pill pill-up");
    expect(statePillClass("stopped")).toBe("pill pill-down");
    expect(statePillClass("degraded")).toBe("pill pill-deg");
    expect(statePillClass("failed")).toBe("pill pill-deg");
    expect(statePillClass("starting")).toBe("pill pill-transit");
    expect(statePillClass("неведомое")).toBe("pill pill-down");
  });
});

describe("bottleActionsLocked", () => {
  it("переходы (starting/stopping) блокируют кнопки", () => {
    expect(bottleActionsLocked("starting")).toBe(true);
    expect(bottleActionsLocked("stopping")).toBe(true);
  });

  it("устойчивые состояния не блокируют", () => {
    for (const state of ["up", "stopped", "degraded", "failed"]) {
      expect(bottleActionsLocked(state)).toBe(false);
    }
  });
});

describe("rowMeta", () => {
  it("при выключенном демоне — «демон выкл.»", () => {
    expect(rowMeta(row({}), true)).toBe("демон выкл.");
  });

  it("env WireGuard работает: протокол + socks5 + пробросы", () => {
    const meta = rowMeta(
      row({ forwards: [{ port: 33389, target: "10.20.30.15:3389" }] }),
      false,
    );
    expect(meta).toBe("WireGuard · socks5 :1080 · :33389→10.20.30.15:3389");
  });

  it("OpenVPN с сохранёнными кредами — пометка в мета", () => {
    const meta = rowMeta(
      row({ protocol: "openvpn", hasCredentials: true, state: "stopped", socks5: null }),
      false,
    );
    expect(meta).toBe("OpenVPN · сохранён логин/пароль");
  });

  it("host-бутылка: «host-клиент»", () => {
    expect(
      rowMeta(row({ kind: "host", protocol: null, socks5: null }), false),
    ).toBe("host-клиент");
  });

  it("причина failed/degraded попадает в мета", () => {
    const meta = rowMeta(
      row({ state: "failed", reason: "handshake timeout", socks5: null }),
      false,
    );
    expect(meta).toBe("WireGuard · handshake timeout");
  });
});
