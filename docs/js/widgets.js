// Menu bar with shaded drop-down menus, and a Win98-style combo box (shaded drop-down list).

/** Menu bar: click a title to open its menu; while one is open, hovering another switches. */
export function initMenus(bar) {
  const titles = [...bar.querySelectorAll("[data-menu]")];
  let open = null;
  const close = () => {
    if (!open) return;
    open.setAttribute("aria-expanded", "false");
    document.getElementById(open.dataset.menu).hidden = true;
    open = null;
  };
  const show = (t) => {
    close();
    open = t;
    t.setAttribute("aria-expanded", "true");
    const menu = document.getElementById(t.dataset.menu);
    if (!menu.classList.contains("menu-right")) menu.style.left = `${t.offsetLeft}px`;
    menu.hidden = false;
    menu.querySelector("a, button")?.focus({ preventScroll: true });
  };
  for (const t of titles) {
    t.addEventListener("click", (e) => {
      e.stopPropagation();
      open === t ? close() : show(t);
    });
    t.addEventListener("mouseenter", () => open && open !== t && show(t));
    const menu = document.getElementById(t.dataset.menu);
    menu.addEventListener("click", (e) => {
      if (e.target.closest("a, button")) close();
    });
    menu.addEventListener("keydown", (e) => {
      const items = [...menu.querySelectorAll("a, button")];
      const i = items.indexOf(document.activeElement);
      if (e.key === "ArrowDown") items[(i + 1) % items.length].focus(), e.preventDefault();
      if (e.key === "ArrowUp") items[(i - 1 + items.length) % items.length].focus(), e.preventDefault();
      if (e.key === "Escape") close(), t.focus();
    });
  }
  document.addEventListener("click", close);
  return { close };
}

/**
 * Combo box. `options`: [{ value, label, detail, group }]. Calls onChange(value).
 * Keyboard: Up/Down/Home/End, Enter to pick, Escape to close, typing jumps to a match.
 */
export function initCombo(root, { options = [], value = null, placeholder = "", onChange = () => {} } = {}) {
  const field = root.querySelector(".combo-field");
  const list = root.querySelector(".combo-list");
  let opts = options;
  let current = value;
  let active = -1;
  let typed = "";
  let typedAt = 0;

  const label = () => {
    const o = opts.find((x) => x.value === current);
    field.querySelector(".combo-value").textContent = o ? `${o.label}${o.detail ? " — " + o.detail : ""}` : placeholder;
  };

  const render = () => {
    list.innerHTML = "";
    let group = null;
    opts.forEach((o, i) => {
      if (o.group !== group) {
        group = o.group;
        const g = document.createElement("li");
        g.className = "combo-group";
        g.setAttribute("role", "presentation");
        g.textContent = group;
        list.append(g);
      }
      const li = document.createElement("li");
      li.id = `${list.id}-${i}`;
      li.setAttribute("role", "option");
      li.setAttribute("aria-selected", String(o.value === current));
      li.dataset.index = i;
      li.innerHTML = `<span class="combo-sym"></span><span class="combo-detail"></span>`;
      li.firstChild.textContent = o.label;
      li.lastChild.textContent = o.detail ?? "";
      if (o.disabled) li.setAttribute("aria-disabled", "true");
      list.append(li);
    });
  };

  const highlight = (i) => {
    list.querySelector(".active")?.classList.remove("active");
    active = i;
    const li = list.querySelector(`[data-index="${i}"]`);
    if (!li) return;
    li.classList.add("active");
    field.setAttribute("aria-activedescendant", li.id);
    li.scrollIntoView({ block: "nearest" });
  };

  const isOpen = () => !list.hidden;
  const openList = () => {
    list.hidden = false;
    field.setAttribute("aria-expanded", "true");
    highlight(Math.max(0, opts.findIndex((o) => o.value === current)));
  };
  const closeList = () => {
    list.hidden = true;
    field.setAttribute("aria-expanded", "false");
  };
  const pick = (i) => {
    const o = opts[i];
    if (!o || o.disabled) return;
    current = o.value;
    render();
    label();
    closeList();
    onChange(current);
  };

  field.addEventListener("click", (e) => {
    e.stopPropagation();
    isOpen() ? closeList() : openList();
  });
  list.addEventListener("click", (e) => {
    e.stopPropagation();
    const li = e.target.closest("[role=option]");
    if (li) pick(+li.dataset.index);
  });
  list.addEventListener("mousemove", (e) => {
    const li = e.target.closest("[role=option]");
    if (li && +li.dataset.index !== active) highlight(+li.dataset.index);
  });
  field.addEventListener("keydown", (e) => {
    const step = (d) => {
      let i = active;
      do i = Math.min(opts.length - 1, Math.max(0, i + d));
      while (opts[i]?.disabled && i > 0 && i < opts.length - 1);
      highlight(i);
    };
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!isOpen()) return openList();
      step(e.key === "ArrowDown" ? 1 : -1);
    } else if (e.key === "Home" && isOpen()) highlight(0), e.preventDefault();
    else if (e.key === "End" && isOpen()) highlight(opts.length - 1), e.preventDefault();
    else if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      isOpen() ? pick(active) : openList();
    } else if (e.key === "Escape") closeList();
    else if (e.key.length === 1) {
      typed = Date.now() - typedAt > 700 ? e.key : typed + e.key;
      typedAt = Date.now();
      const i = opts.findIndex((o) => o.label.toLowerCase().startsWith(typed.toLowerCase()));
      if (i >= 0) (isOpen() ? highlight(i) : pick(i));
    }
  });
  document.addEventListener("click", closeList);

  render();
  label();
  return {
    setOptions(next, val = current) {
      opts = next;
      current = val;
      render();
      label();
    },
    get value() {
      return current;
    },
  };
}
