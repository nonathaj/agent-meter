// agent-meter for the Herdr app.
//
// Under the sidebar's footer: the account each provider is on now, and how
// full it is. Clicking it opens every account with each of its limits, what
// agent-meter can do with one, and agent-meter's settings. A switch is
// announced, whoever made it.
//
// Everything comes from agent-meter's own commands (`list --json`,
// `history --json`, `config show --json`, `spend-reset --dry-run --json`), and
// every change goes through the commands a person would type. Nothing here
// asks a provider anything: polling stays with `agent-meter watch`.
//
// Installed by `agent-meter herdr install`, which points AGENT_METER at
// itself. Edits here are overwritten by the next install.

import { View, div, image, svg } from "gpui-kit";
import { v_flex, h_flex, ScrollHandle } from "gpui-base";
import { agents, exec, run, theme, props, layout, openOverlay, closeOverlay, notify, watchFolder, checking } from "herdr";

const AGENT_METER = "agent-meter";

const PROVIDERS = [
  { key: "claude", name: "Claude Code", short: "Claude", app: ["claude-code", "claude"], icon: "icons/claude.svg" },
  { key: "codex", name: "Codex", short: "Codex", app: ["codex"], icon: "icons/codex.svg" },
];
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
// The footer's own rows are 28 px; the block keeps 8 px under the last one.
const ROW = 28;

function find(node, id) {
  if (node?.id === id) return node;
  for (const child of node?.children ?? []) {
    const found = find(child, id);
    if (found) return found;
  }
  return null;
}
const narrow = () => (find(layout(), "sidebar")?.size?.px ?? Infinity) < 160;

function span(ms) {
  const m = Math.max(1, Math.round(ms / 60000));
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h${String(m % 60).padStart(2, "0")}m`;
  return `${Math.floor(h / 24)}d${h % 24}h`;
}
const day = (iso) => { const d = new Date(iso); return `${MONTHS[d.getMonth()]} ${d.getDate()}`; };
const sentence = (text) => (text ? text[0].toUpperCase() + text.slice(1) : text);

const used = (a) => Math.round(a.usage?.usedPercent ?? 0);
const level = (pct, threshold) => (pct >= threshold ? "hot" : pct >= threshold - 20 ? "warn" : "ok");
const provider = (key) => PROVIDERS.find(p => p.key === key);

// Who an account is, as the person knows it: their own label, else the email
// they signed in with. agent-meter's ids (`claude-7`) are for the CLI.
const who = (a) => a?.label || a?.email || a?.id || "";
// Its plan as the provider sells it: "Max 20x", "Team", "Free".
const plan = (a) => {
  const p = (a?.plan ?? "").trim();
  if (!p || p === "-") return "";
  const m = a.capacityMultiplier > 1 ? ` ${a.capacityMultiplier}x` : "";
  return p[0].toUpperCase() + p.slice(1) + m;
};
const named = (a) => (plan(a) ? `${who(a)} (${plan(a)})` : who(a));

// Runs agent-meter for its JSON, or the reason there is none in `error`. A
// command that answered in JSON is taken at its word whatever it exits with:
// a reset that wasn't spent says why there.
async function agentMeter(args) {
  const r = await exec(AGENT_METER, args);
  try {
    return { json: JSON.parse(r.stdout) };
  } catch {
    return { error: r.stderr.trim().split("\n")[0] || `agent-meter ${args[0]} failed` };
  }
}

export default class AgentMeter extends View {
  init(_props, cx) {
    this.accounts = null;
    this.settings = null;
    this.error = null;
    this.reading = false;
    this.page = "accounts";
    this.settingsScroll = ScrollHandle.new();
    this.offers = null;
    this.choice = 0;
    // The latest switch already seen, so only newer ones are announced.
    this.seen = null;
    const p = props() ?? {};
    if (checking() || p.dialog === "menu") return;
    if (p.dialog === "spend") { this.readOffers(cx, p.id); return; }
    this.refresh(cx);
    if (!p.compact) this.readSettings(cx);
    cx.timer.every(30000, (cx) => this.refresh(cx));
    // Every switch and reading lands in agent-meter's own folder: follow it.
    cx.spawn(async (cx) => {
      const r = await exec(AGENT_METER, ["where"]);
      const where = r.stdout.trim();
      if (r.code !== 0 || !where) return;
      for (;;) {
        await watchFolder(where);
        this.refresh(cx);
        if (!props()?.compact) this.readSettings(cx);
      }
    });
  }

  refresh(cx, poll = false) {
    cx.spawn(async (cx) => {
      if (poll) { this.reading = true; cx.notify(); }
      const r = await agentMeter(poll ? ["list", "--poll", "--json"] : ["list", "--json"]);
      this.reading = false;
      if (r.error) { this.error = r.error; cx.notify(); return; }
      this.accounts = r.json;
      this.error = null;
      // The footer copy is always there, so it is the one that announces.
      if (props()?.compact) { await this.announce(); this.fit(); }
      cx.notify();
    });
  }

  readSettings(cx) {
    cx.spawn(async (cx) => {
      const r = await agentMeter(["config", "show", "--json"]);
      if (r.json) this.settings = r.json.settings;
      cx.notify();
    });
  }

  // Changes one setting as `agent-meter config set` (or `unset`) does.
  change(cx, args) {
    cx.spawn(async (cx) => {
      const r = await exec(AGENT_METER, ["config", ...args]);
      this.error = r.code === 0 ? null : (r.stderr.trim().split("\n")[0] || `Could not change ${args[1]}`);
      this.readSettings(cx);
      this.refresh(cx);
    });
  }

  // Each switch made since the last look, as a notice: where it went, what
  // kind of account that is, and why the old one was left. A toast holds one
  // short line of each.
  async announce() {
    const r = await agentMeter(["history", "--json", "-n", "5"]);
    if (!r.json) return;
    const latest = r.json.at(-1)?.at ?? "";
    const fresh = this.seen === null ? [] : r.json.filter(s => s.at > this.seen);
    this.seen = latest;
    const byId = (id) => (this.accounts ?? []).find(a => a.id === id);
    for (const s of fresh) {
      const p = provider(s.provider);
      const now = byId(s.to);
      const was = s.from ? (byId(s.from) ?? { id: s.from }) : null;
      const why = s.by === "hand" ? "switched by hand"
        : s.why === "threshold" && s.used != null ? `${who(was)} hit ${Math.round(s.used)}%`
        : s.why === "least_used" ? "every account is past the mark"
        : s.why === "exhausted" ? `${who(was)} ran out`
        : s.why === "signed_out" ? `${who(was)} was signed out`
        : s.why === "week_ends_sooner" ? "its week ends sooner"
        : "";
      notify({
        title: `Switched to ${who(now ?? { id: s.to })}`,
        body: [[p?.short, plan(now)].filter(Boolean).join(" "), why].filter(Boolean).join(" · "),
        level: "info",
        icon: p?.icon,
        action: { name: "mod.open", args: { name: "agent-meter" } },
      });
    }
  }

  // One row per provider with an account in use: size the block to them,
  // and the column it shares with the footer.
  fit() {
    const rows = this.inUse().length;
    const px = rows ? rows * ROW + 8 : 0;
    const footer = find(layout(), "sidebar-footer")?.size?.px;
    if (footer == null || find(layout(), "accounts")?.size?.px === px) return;
    run("layout.resize", { id: "accounts", px });
    run("layout.resize", { id: "sidebar-bottom", px: footer + px });
  }

  inUse() {
    const list = this.accounts ?? [];
    return PROVIDERS.map(p => list.find(a => a.provider === p.key && a.active)).filter(Boolean);
  }

  // The mark a provider's accounts are judged by, as agent-meter reports it.
  thresholdFor(key) {
    return (this.accounts ?? []).find(a => a.provider === key)?.threshold ?? 90;
  }

  colors(t) {
    return { ok: t.ansi2 ?? t.idle, warn: t.ansi3 ?? t.blocked, hot: t.ansi1 ?? t.blocked };
  }

  render() {
    const t = theme();
    const p = props() ?? {};
    if (p.dialog === "spend") return this.spendDialog(t, p);
    if (p.dialog === "menu") return this.menu(t, p);
    if (p.compact) return this.footer(t);
    return this.all(t, p);
  }

  // Under the footer: the account each provider is on now, with its fullest limit.
  footer(t) {
    const c = this.colors(t);
    const collapsed = narrow();
    const open = () => run("mod.open", { name: "agent-meter" });
    return v_flex().size_full().bg(t.chrome).px(8).pb(8).text_size(13)
      .when(collapsed, el => el.px(0).items_center())
      .children(this.inUse().map(a => {
        const pct = used(a);
        const lv = level(pct, a.threshold ?? 90);
        const prov = provider(a.provider);
        const tip = `${prov.name}: ${named(a)}, ${pct}% used`;
        if (collapsed) {
          return div().id(`am-f-${a.id}`).relative().w(32).h(ROW).flex().items_center().justify_center().rounded(8)
            .hover(s => s.bg(t.hover)).cursor_pointer().tooltip(tip).on_click(open)
            .child(image(prov.icon).w(14).h(14))
            .child(div().absolute().top(5).right(6).size(6).rounded_full().bg(c[lv]));
        }
        return h_flex().id(`am-f-${a.id}`).h(ROW).px(8).gap(9).items_center().rounded(8)
          .text_color(t.muted).hover(s => s.bg(t.hover).text_color(t.foreground)).cursor_pointer()
          .tooltip(tip).on_click(open)
          .child(image(prov.icon).w(14).h(14).flex_none())
          .child(div().flex_1().min_w(0).truncate().child(who(a)))
          .child(div().flex_none().w(24).h(3).rounded(2).bg(t.border)
            .child(div().h_full().w(`${Math.min(100, Math.max(4, pct))}%`).rounded(2).bg(c[lv])))
          .child(div().flex_none().w(28).text_right().text_size(12).text_color(c[lv]).child(`${pct}%`));
      }));
  }

  // The modal: every account and every limit it has, or the settings.
  all(t, p) {
    const list = this.accounts ?? [];
    const live = agents();
    const c = this.colors(t);
    const now = Date.now();

    // One limit: its name, a bar, how full, and when it starts over.
    const limit = (a, w) => {
      const pct = Math.round(w.usedPercent);
      const color = w.counted ? c[level(pct, a.threshold ?? 90)] : t.dim;
      return h_flex().h(18).gap(10).items_center().text_size(11.5)
        .when(!w.counted, el => el.tooltip("Not counted for switching"))
        .child(div().flex_none().w(96).truncate().text_color(w.counted ? t.muted : t.dim).child(w.label))
        .child(div().flex_1().h(4).rounded(2).bg(t.border)
          .child(div().h_full().w(`${Math.min(100, Math.max(1.5, pct))}%`).rounded(2).bg(color)))
        .child(div().flex_none().w(36).text_right().text_size(12).font_medium().text_color(color).child(`${pct}%`))
        .child(div().flex_none().w(104).text_color(t.dim)
          .when(Boolean(w.resetsAt), el => el.tooltip(`${day(w.resetsAt)}`).child(`resets in ${span(Date.parse(w.resetsAt) - now)}`)));
    };

    const row = (prov, a) => {
      const resets = a.usage?.limitResets;
      const on = a.active ? live.filter(x => prov.app.includes(x.app ?? "")) : [];
      const windows = a.usage?.windows ?? [];
      const notes = [
        resets?.available ? `${resets.available} limit reset${resets.available === 1 ? "" : "s"}${resets.nextExpiresAt ? `, the first expires ${day(resets.nextExpiresAt)}` : ""}` : null,
        a.disabled ? (a.disabled.until ? `left out of switching for ${span(Date.parse(a.disabled.until) - now)}` : "left out of switching") : null,
        a.needsLogin ? "needs signing in again" : null,
      ].filter(Boolean).join(" · ");
      return v_flex().id(`am-${a.id}`).px(10).py(7).gap(4).flex_none().rounded(8)
        .when(a.active, el => el.bg(t.selected))
        .cursor_pointer().hover(s => s.bg(t.hover))
        .on_click(() => this.openMenu(a, "pointer"))
        .child(h_flex().h(20).gap(8).items_center()
          .child(div().flex_none().max_w(230).truncate().text_color(a.active ? t.foreground : t.muted).font_medium().child(who(a)))
          .when(Boolean(plan(a)), el => el.child(div().flex_none().text_size(11.5).text_color(t.dim).child(plan(a))))
          .when(a.active, el => el.child(div().flex_none().px(6).h(17).rounded(5).bg(t.accent + "2e").text_size(10.5).font_semibold().text_color(t.accent).flex().items_center().child("In use")))
          .when(a.queue?.role === "next_up", el => el.child(div().flex_none().px(6).h(17).rounded(5).border(1).border_color(t.border).text_size(10.5).font_medium().text_color(t.muted).flex().items_center()
            .tooltip("Where agent-meter switches when this provider's account fills up").child("Next up")))
          .child(div().flex_1())
          .when(on.length > 0, el => el.child(h_flex().flex_none().gap(3).items_center().text_size(11).text_color(t.dim)
            .tooltip(on.map(x => `${x.space_name}${x.tab_name ? ` · ${x.tab_name}` : ""}`).join("\n"))
            .child(image(prov.icon).w(12).h(12)).child(`${on.length} agent${on.length === 1 ? "" : "s"}`)))
          .child(div().flex_none().text_size(11).text_color(t.dim).tooltip("agent-meter's id for it").child(a.id)))
        .when(windows.length === 0, el => el.child(div().text_size(11.5).text_color(t.dim).child(a.error ? `No reading: ${a.error}` : "No reading yet")))
        .children(windows.map(w => limit(a, w)))
        .when(Boolean(notes), el => el.child(div().pt(1).truncate().text_size(11.5).text_color(t.dim).child(notes)));
    };

    const group = (prov) => {
      // In the order agent-meter would switch through them, as its own UI lists them:
      // the one in use, the next one up, then the rest; one it can't use goes last.
      const place = (a) => a.queue?.position ?? Number.MAX_SAFE_INTEGER;
      const mine = list.filter(a => a.provider === prov.key).sort((x, y) => place(x) - place(y));
      if (!mine.length) return [];
      const mark = this.thresholdFor(prov.key);
      const room = mine.filter(a => used(a) < mark && !a.disabled).length;
      return [
        h_flex().id(`am-h-${prov.key}`).px(10).pt(12).pb(4).flex_none().gap(7).items_center().text_size(11.5).font_medium()
          .child(image(prov.icon).w(14).h(14))
          .child(div().text_color(t.muted).child(prov.name))
          .child(div().flex_1())
          .child(div().text_color(t.dim).child(`${room} of ${mine.length} below ${mark}%`)),
        ...mine.map(a => row(prov, a)),
      ];
    };

    const settings = this.page === "settings";
    const icon = (id, child, tip, onClick) => div().id(id).size(26).flex_none().flex().items_center().justify_center().rounded(6)
      .text_color(t.dim).cursor_pointer().hover(s => s.bg(t.hover).text_color(t.foreground)).tooltip(tip).on_click(onClick)
      .child(child);
    const glyph = (name) => svg(`mods/agent-meter/${name}.svg`).size(14);
    return v_flex().size_full().min_h(0).bg(t.popover).text_size(13)
      .child(h_flex().pl(settings ? 10 : 18).pr(12).h(48).flex_none().gap(4).items_center().border_b(1).border_color(t.border)
        .when(settings, el => el.child(icon("am-back", glyph("chevron-left"), "Accounts", (_e, cx) => { this.page = "accounts"; cx.notify(); })))
        .child(div().flex_1().pl(settings ? 4 : 0).text_size(14).font_semibold().text_color(t.foreground).child(settings ? "Settings" : "Accounts"))
        .when(!settings, el => el
          .child(icon("am-refresh", div().text_size(14).child(this.reading ? "…" : "↻"), "Read usage again, where it is due",
            (_e, cx) => { if (!this.reading) this.refresh(cx, true); }))
          .child(icon("am-settings", glyph("settings"), "Settings", (_e, cx) => { this.page = "settings"; this.readSettings(cx); cx.notify(); })))
        .when(Boolean(p.overlay), el => el.child(icon("am-close", svg("mods/pane/close.svg").size(13), "Close", () => closeOverlay(p.overlay)))))
      .when(Boolean(this.error), el => el.child(div().px(18).pt(10).text_size(12).text_color(t.blocked).child(this.error)))
      .when(this.accounts === null && !this.error, el => el.child(div().px(18).pt(10).text_size(12).text_color(t.dim).child("Reading accounts…")))
      .when(!settings && this.accounts?.length === 0, el => el.child(div().px(18).pt(10).text_size(12).text_color(t.dim)
        .child("No accounts yet. Run `agent-meter import` to store the one you are signed in to.")))
      .when(settings, el => el.child(this.settingsView(t)))
      .when(!settings, el => el.child(v_flex().id("am-list").flex_1().min_h(0).px(8).pb(10).overflow_y_scrollbar().children(PROVIDERS.flatMap(group))));
  }

  // agent-meter's settings, as `config show --json` lists them: those every
  // agent shares, then each provider that has accounts. Each says what every
  // change would set it to, so nothing here works a step out.
  settingsView(t) {
    const all = this.settings ?? [];
    const set = (key, value) => (_e, cx) => this.change(cx, ["set", key, value]);

    const button = (id, name, value, key) => div().id(id).w(28).h_full().flex().items_center().justify_center()
      .text_color(value != null ? t.muted : t.border)
      .when(value != null, el => el.cursor_pointer().hover(s => s.bg(t.hover).text_color(t.foreground)).on_click(set(key, value)))
      .child(svg(`mods/agent-meter/${name}.svg`).size(12));
    const stepper = (s, id) => h_flex().flex_none().h(28).rounded(7).border(1).border_color(t.border).overflow_hidden()
      .child(button(`${id}-down`, "minus", s.down, s.key))
      .child(div().w(150).h_full().px(6).flex().items_center().justify_center().border_l(1).border_r(1).border_color(t.border)
        .text_size(12.5).text_color(s.own ? t.foreground : t.muted).child(div().truncate().child(s.own ? s.value : s.value.replace(/, as every agent$/, ""))))
      .child(button(`${id}-up`, "plus", s.up, s.key));
    const toggle = (s, id) => div().id(id).flex_none().relative().w(34).h(20).rounded_full().cursor_pointer()
      .bg(s.on ? t.accent : t.border).on_click(set(s.key, s.toggle))
      .child(div().absolute().top(3).left(s.on ? 17 : 3).size(14).rounded_full().bg(s.on ? "#ffffff" : t.muted));
    const choice = (s, id) => h_flex().flex_none().h(28).p(2).gap(2).rounded(7).border(1).border_color(t.border)
      .children(s.choices.map((o, i) => div().id(`${id}-${i}`).h_full().px(10).flex().items_center().rounded(5).text_size(12.5).cursor_pointer()
        .when(o.chosen, el => el.bg(t.selected).text_color(t.foreground))
        .when(!o.chosen, el => el.text_color(t.muted).hover(s => s.bg(t.hover)).on_click(set(s.key, o.set)))
        .child(o.value)));
    const pill = (id, text, tip, onClick) => div().id(id).flex_none().px(6).h(17).rounded(5).border(1).border_color(t.border).flex().items_center()
      .text_size(10.5).text_color(t.dim).tooltip(tip)
      .when(Boolean(onClick), el => el.cursor_pointer().hover(s => s.text_color(t.foreground)).on_click(onClick)).child(text);

    const shared = all.find(s => s.key === "watch.threshold");
    const row = (s, i) => {
      const id = `s-${i}`;
      const control = s.kind === "number" ? stepper(s, id) : s.kind === "switch" ? toggle(s, id) : choice(s, id);
      // A provider's own mark says whether it has one, and offers the shared one back.
      const own = s.provider && s.kind === "number" ? (s.own
        ? pill(`${id}-p`, `every agent's is ${shared?.value ?? ""}`.replace(/ used$/, ""), "Use every agent's mark again",
          (_e, cx) => this.change(cx, ["unset", s.key]))
        : pill(`${id}-p`, "as every agent", `Every agent's mark: ${shared?.value ?? ""}`)) : null;
      return h_flex().id(id).px(10).py(9).gap(18).items_center().flex_none()
        .child(v_flex().flex_1().min_w(0).gap(3)
          .child(h_flex().gap(8).items_center().child(div().text_color(t.foreground).child(s.label)).when(Boolean(own), el => el.child(own)))
          // Backticks mark a command for a terminal; here they would show as they are.
          .child(div().text_size(11.5).text_color(t.dim).child(s.explanation.replaceAll("`", ""))))
        .child(control);
    };
    const heading = (id, text, icon) => h_flex().id(id).px(10).pt(14).pb(2).flex_none().gap(7).items_center().text_size(11.5).font_medium().text_color(t.muted)
      .when(Boolean(icon), el => el.child(image(icon).w(14).h(14))).child(text);

    const children = [];
    let group;
    all.forEach((s, i) => {
      if (i === 0 || s.provider !== group) {
        group = s.provider;
        const p = provider(s.provider);
        children.push(heading(`s-h-${s.provider ?? "all"}`, p?.name ?? "Every agent", p?.icon));
      }
      children.push(row(s, i));
    });

    return v_flex().id("am-settings").flex_1().min_h(0).px(8).pb(12).overflow_y_scrollbar().track_scroll(this.settingsScroll)
      .when(this.settings === null, el => el.child(div().px(10).pt(12).text_size(12).text_color(t.dim).child("Reading settings…")))
      .children(children);
  }

  openMenu(a, at) {
    if (!a) return;
    openOverlay({ mod: "agent-meter", kind: "popup", at, width: 290, height: 176, props: { dialog: "menu", id: a.id, account: a } });
  }

  openSpend(a) {
    if (!a) return;
    openOverlay({ mod: "agent-meter", kind: "dialog", width: 480, height: 290, props: { dialog: "spend", id: a.id, account: a } });
  }

  // What can be done with one account.
  menu(t, p) {
    const a = p.account;
    const item = (id, text, onClick, dim = false) => h_flex().id(id).h(30).px(10).rounded(6).items_center().cursor_pointer()
      .text_color(dim ? t.dim : t.foreground).hover(s => s.bg(t.hover)).on_click(onClick).child(text);
    const close = () => closeOverlay(p.overlay);
    // What went wrong, where the command says.
    const act = (args) => exec(AGENT_METER, args).then(r => {
      if (r.code !== 0) notify({ title: `agent-meter ${args[0]} failed`, body: r.stderr.trim().split("\n")[0], level: "warning" });
    });
    const resets = a?.usage?.limitResets?.available ?? 0;
    return v_flex().size_full().p(6).gap(1).text_size(13).bg(t.popover)
      .child(div().px(10).pt(4).pb(6).truncate().text_size(11).text_color(t.dim).child(named(a)))
      .child(item("m-use", a?.active ? "In use" : "Use this account", () => { close(); if (!a?.active) act(["use", p.id]); }, a?.active))
      .child(item("m-spend", resets ? `Spend a limit reset… (${resets})` : "No limit resets", () => { close(); if (resets) this.openSpend(a); }, !resets))
      .child(item("m-off5", "Leave out of switching for 5 hours", () => { close(); act(["disable", p.id, "--for", "5h"]); }))
      .child(item("m-off", a?.disabled ? "Take into switching again" : "Leave out until I take it back", () => {
        close(); act(a?.disabled ? ["enable", p.id] : ["disable", p.id]);
      }));
  }

  readOffers(cx, id) {
    cx.spawn(async (cx) => {
      const r = await agentMeter(["spend-reset", id, "--dry-run", "--json"]);
      this.offers = r.json?.offers ?? [];
      this.error = r.error ?? null;
      cx.notify();
    });
  }

  // Spending a reset: agent-meter's own words for what it would do, then ask.
  spendDialog(t, p) {
    const a = p.account;
    const offers = (this.offers ?? []).filter(o => o.grant);
    const o = offers[Math.min(this.choice, offers.length - 1)];
    const icon = provider(a?.provider)?.icon ?? "icons/agent.svg";
    const button = (id, text, solid, onClick) => div().id(id).h(32).px(14).rounded(7).flex().items_center().cursor_pointer()
      .text_size(13).font_medium()
      .when(solid, el => el.bg(t.solid).text_color(t.solid_foreground))
      .when(!solid, el => el.border(1).border_color(t.border).text_color(t.foreground).hover(s => s.bg(t.hover)))
      .on_click(onClick).child(text);
    return v_flex().size_full().p(20).gap(12).bg(t.popover).text_size(13)
      .child(h_flex().gap(10).items_center()
        .child(image(icon).w(18).h(18).flex_none())
        .child(div().min_w(0).truncate().text_size(15).font_semibold().text_color(t.foreground).child(`Spend a limit reset on ${who(a)}?`)))
      .child(div().text_color(t.dim).child([provider(a?.provider)?.name, plan(a)].filter(Boolean).join(" · ")))
      .when(this.offers === null, el => el.child(div().text_color(t.dim).child("Reading its resets…")))
      .when(Boolean(this.error), el => el.child(div().text_color(t.blocked).child(this.error)))
      .when(this.offers !== null && !offers.length && !this.error, el => el.child(div().text_color(t.dim).child("The last reading holds no reset to spend.")))
      .child(v_flex().gap(2).children(offers.map((x, i) => h_flex().id(`g-${i}`).h(28).px(8).gap(8).rounded(6).items_center().cursor_pointer()
        .when(x === o, el => el.bg(t.selected)).hover(s => s.bg(t.hover))
        .on_click((_e, cx) => { this.choice = i; cx.notify(); })
        .child(div().size(8).rounded(4).border(1).border_color(x === o ? t.accent : t.input).when(x === o, el => el.bg(t.accent)))
        .child(div().min_w(0).truncate().text_color(x === o ? t.foreground : t.muted).child(x.title)))))
      .when(Boolean(o), el => el.child(v_flex().gap(3).px(8).text_size(12.5).text_color(t.muted)
        .children(o.lines.map((line, i) => div().when(!o.usable && i === o.lines.length - 1, el => el.text_color(t.blocked)).child(sentence(line))))))
      .child(div().flex_1())
      .child(h_flex().gap(8).items_center()
        .child(div().flex_1().text_size(12).text_color(t.dim).child("A reset is spent once and can't be undone."))
        .child(button("cancel", "Cancel", false, () => closeOverlay(p.overlay)))
        .when(Boolean(o), el => el.child(button("spend", "Spend reset", true, () => {
          closeOverlay(p.overlay);
          agentMeter(["spend-reset", p.id, "--grant", o.grant, "--yes", "--json"]).then(r => notify({
            title: r.json?.spent ? `Spent a limit reset on ${who(a)}` : `No reset was spent on ${who(a)}`,
            body: r.json?.spent ? r.json.after : (r.json?.outcome ?? r.error),
            level: r.json?.spent ? "success" : "warning",
            icon,
          }));
        }))));
  }
}
