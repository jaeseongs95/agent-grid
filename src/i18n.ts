export type Language = 'ko' | 'en';

const storageKey = 'agent-grid-language';
let language: Language = 'ko';
let storageError: unknown;
try { language = localStorage.getItem(storageKey) === 'en' ? 'en' : 'ko'; }
catch (error) { storageError = error; }
if (typeof document !== 'undefined') document.documentElement.lang = language;

const listeners = new Set<() => void>();
const bindings = new WeakMap<Node, Map<string, [string, string]>>();
const boundNodes = new Set<WeakRef<Node>>();
const cleanup = new FinalizationRegistry<WeakRef<Node>>(reference => boundNodes.delete(reference));
export function getLanguage(): Language { return language; }
export function t(ko: string, en: string): string { return language === 'ko' ? ko : en; }
export function locale(): string { return language === 'ko' ? 'ko-KR' : 'en-US'; }
export function languageStorageError(): unknown { return storageError; }
export function onLanguageChange(listener: () => void): () => void { listeners.add(listener); return () => listeners.delete(listener); }

function bind(node: Node, attribute: string, ko: string, en: string) {
  let values = bindings.get(node);
  if (!values) { values = new Map(); bindings.set(node, values); const reference = new WeakRef(node); boundNodes.add(reference); cleanup.register(node, reference); }
  values.set(attribute, [ko, en]);
  if (attribute) (node as Element).setAttribute(attribute, t(ko, en));
  else node.textContent = t(ko, en);
}
export function bindText(node: Node, ko: string, en: string): void { bind(node, '', ko, en); }
export function bindAttribute(element: Element, attribute: string, ko: string, en: string): void { bind(element, attribute, ko, en); }
export function unbindText(node: Node): void { bindings.get(node)?.delete(''); }

export function setLanguage(value: Language): void {
  if (value !== 'ko' && value !== 'en') throw new Error('Unsupported language');
  if (language !== value) {
    language = value;
    if (typeof document !== 'undefined') document.documentElement.lang = language;
    for (const reference of boundNodes) {
      const node = reference.deref();
      if (!node || !node.isConnected) { boundNodes.delete(reference); if (node) bindings.delete(node); continue; }
      const values = bindings.get(node)!;
      for (const [attribute, [ko, en]] of values) {
        if (attribute) (node as Element).setAttribute(attribute, t(ko, en));
        else node.textContent = t(ko, en);
      }
    }
    for (const listener of listeners) listener();
  }
  try { localStorage.setItem(storageKey, value); storageError = undefined; }
  catch (error) { storageError = error; throw error; }
}
