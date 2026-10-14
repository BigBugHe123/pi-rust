import { initI18n } from './i18n.js?v=20261014-language-fix';

await initI18n();

const toggle = document.querySelector('[data-menu-toggle]');
const menu = document.querySelector('[data-mobile-menu]');
toggle.addEventListener('click', () => {
  const open = menu.classList.toggle('is-open');
  toggle.setAttribute('aria-expanded', String(open));
});
menu.querySelectorAll('a').forEach((link) => link.addEventListener('click', () => {
  menu.classList.remove('is-open');
  toggle.setAttribute('aria-expanded', 'false');
}));
