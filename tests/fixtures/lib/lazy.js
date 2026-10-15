await new Promise((r) => setTimeout(r, 1));
export const loaded = "lazy module evaluated after TLA";
export const meta = import.meta.main;
