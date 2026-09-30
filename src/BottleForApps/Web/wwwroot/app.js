const state = { bottles: [], overview: null, polling: null, tourStep: 0 };
const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];
const icons = {
  bottle: '<svg viewBox="0 0 24 24"><path d="M9 2h6v3l-1 2v2c3 1 5 3.8 5 7v5H5v-5c0-3.2 2-6 5-7V7L9 5V2Z"/><path d="M7 16c2.5-1.5 7.5-1.5 10 0"/></svg>',
  screen: '<svg viewBox="0 0 24 24"><rect x="3" y="4" width="18" height="13" rx="2"/><path d="M8 21h8M12 17v4"/></svg>',
  route: '<svg viewBox="0 0 24 24"><circle cx="6" cy="18" r="2"/><circle cx="18" cy="6" r="2"/><path d="M8 18h2a2 2 0 0 0 2-2V8a2 2 0 0 1 2-2h2"/></svg>',
  play: '<svg viewBox="0 0 24 24"><path d="m8 5 11 7-11 7V5Z"/></svg>',
  stop: '<svg viewBox="0 0 24 24"><rect x="6" y="6" width="12" height="12" rx="1"/></svg>',
  edit: '<svg viewBox="0 0 24 24"><path d="m14 5 5 5M4 20l3.5-.7L19 7a2.1 2.1 0 0 0-3-3L3.7 16.5 3 20h1Z"/></svg>',
  trash: '<svg viewBox="0 0 24 24"><path d="M4 7h16M9 7V4h6v3M7 7l1 13h8l1-13M10 11v5M14 11v5"/></svg>'
};

async function api(path, options = {}) {
  const response = await fetch(path, { ...options, headers: options.body ? { 'Content-Type': 'application/json', ...(options.headers || {}) } : options.headers });
  if (!response.ok) {
    let message = `Ошибка ${response.status}`;
    try { message = (await response.json()).message || message; } catch {}
    throw new Error(message);
  }
  return response.status === 204 ? null : response.json();
}

async function loadAll(showSkeleton = false) {
  if (showSkeleton) $('#profilesGrid').innerHTML = '<div class="skeleton"></div><div class="skeleton"></div>';
  try {
    const [overview, bottles] = await Promise.all([api('/api/overview'), api('/api/bottles')]);
    state.overview = overview; state.bottles = bottles;
    renderOverview(); renderProfiles();
    const running = bottles.some(x => ['starting', 'active'].includes(x.session?.state));
    clearTimeout(state.polling);
    if (running) state.polling = setTimeout(loadAll, 1600);
  } catch (error) { showToast(error.message, 'error'); renderLoadError(); }
}

function renderOverview() {
  const o = state.overview;
  $('#totalCount').textContent = o.total;
  $('#activeCount').textContent = o.active;
  $('#healthText').textContent = o.supported && o.networkAvailable ? 'Готово к работе' : o.supported ? 'Сеть недоступна' : 'ОС не поддерживается';
  $('#networkLabel').textContent = o.networkAvailable ? 'Сеть доступна' : 'Нет подключения';
  const helperSuffix = o.routeHelper === undefined ? '' : o.routeHelper ? ' · служба маршрутов: ок' : ' · служба маршрутов: нет';
  $('#platformLabel').textContent = o.platform + helperSuffix;
  $('#networkDot').classList.toggle('online', o.networkAvailable);
}

function renderProfiles() {
  const grid = $('#profilesGrid');
  $('#profileCount').textContent = state.bottles.length ? `${state.bottles.length} ${plural(state.bottles.length, 'профиль', 'профиля', 'профилей')}` : '';
  if (!state.bottles.length) {
    grid.innerHTML = `<div class="empty-state"><div><span class="empty-graphic">${icons.bottle}</span><h3>Создайте первое подключение</h3><p>Добавьте VPN-клиент и адрес удалённого компьютера. В следующий раз подключение запустится одной кнопкой.</p><div class="empty-actions"><button class="button button-primary" data-action="create">Создать профиль</button><button class="button button-secondary" data-action="start-guide">Сначала показать как</button></div></div></div>`;
    return;
  }
  grid.innerHTML = state.bottles.map(cardTemplate).join('');
}

function cardTemplate(b) {
  const status = b.session?.state || 'idle';
  const active = ['starting', 'active'].includes(status);
  const label = status === 'starting' ? 'Подключение' : status === 'active' ? 'Активно' : status === 'error' ? 'Ошибка' : 'Готов';
  const lastMessage = b.session?.messages?.at(-1);
  const routes = b.session?.routes || [];
  return `<article class="profile-card ${active ? 'active-session' : ''}" data-id="${escapeHtml(b.id)}">
    <div class="card-top"><span class="profile-icon">${icons.bottle}</span><span class="pill ${active ? 'active' : status === 'error' ? 'error' : ''}">${label}</span></div>
    <h3>${escapeHtml(b.name)}</h3><div class="endpoint">${escapeHtml(b.rdpHost)}:${b.rdpPort}</div>
    <div class="card-meta"><span>${icons.route}<em>${escapeHtml(b.vpnEndpoints[0])}${b.vpnEndpoints.length > 1 ? ` +${b.vpnEndpoints.length - 1}` : ''}</em></span><span>${icons.route}<em>${escapeHtml(b.baseInterface || 'Физическая сеть: авто')}</em></span><span>${icons.screen}<em>${escapeHtml(b.vpnInterface || 'VPN: авто')}</em></span></div>
    ${routes.length ? `<div class="route-paths">${routes.map(route => `<div class="route-path ${route.verified ? 'verified' : 'failed'}"><span>${escapeHtml(route.addressFamily)}</span><code>${escapeHtml(route.address)} → ${escapeHtml(route.interfaceName)} → ${escapeHtml(route.gateway)}</code><b>${route.verified ? 'Проверен' : 'Ошибка'}</b></div>`).join('')}</div>` : ''}
    <div class="card-actions">
      <button class="button ${active ? 'button-danger' : 'button-primary'} connect" data-action="${active ? 'stop' : 'start'}">${active ? icons.stop + ' Остановить' : icons.play + ' Подключиться'}</button>
      <button class="icon-button" data-action="edit" title="Изменить" ${active ? 'disabled' : ''}>${icons.edit}</button>
      <button class="icon-button danger" data-action="delete" title="Удалить" ${active ? 'disabled' : ''}>${icons.trash}</button>
    </div>
    ${lastMessage ? `<div class="session-log" title="${escapeHtml(lastMessage)}">${escapeHtml(lastMessage)}</div>` : ''}
  </article>`;
}

function renderLoadError() { $('#profilesGrid').innerHTML = '<div class="empty-state"><div><h3>Не удалось загрузить данные</h3><p>Проверьте, что приложение продолжает работать, и попробуйте снова.</p><button class="button button-secondary" data-action="refresh">Повторить</button></div></div>'; }

async function loadNetwork() {
  const list = $('#networkList');
  list.innerHTML = '<div class="skeleton"></div>';
  try {
    const items = await api('/api/interfaces');
    $('#baseInterfaceOptions').innerHTML = items.filter(a => a.physicalCandidate).map(a => `<option value="${escapeHtml(a.name)}">${escapeHtml(a.description)}</option>`).join('');
    list.innerHTML = items.map(a => `<article class="network-card"><div><h3>${escapeHtml(a.name)}</h3><p title="${escapeHtml(a.description)}">${escapeHtml(a.description)}</p></div><div class="network-stat"><span>IPv4 / IPv6</span><strong>${escapeHtml([...a.addresses, ...a.ipv6Addresses].join(', ') || '—')}</strong></div><div class="network-stat"><span>Шлюз</span><strong>${escapeHtml([a.gateway, a.ipv6Gateway].filter(Boolean).join(', ') || '—')}</strong></div><span class="pill ${a.status === 'Up' ? 'active' : ''}">${a.status === 'Up' ? 'Работает' : 'Неактивен'}</span></article>`).join('') || '<div class="empty-state"><div><h3>Интерфейсы не найдены</h3></div></div>';
  } catch (error) { list.innerHTML = ''; showToast(error.message, 'error'); }
}

async function loadBaseInterfaceOptions() {
  try {
    const items = await api('/api/interfaces');
    $('#baseInterfaceOptions').innerHTML = items.filter(a => a.physicalCandidate).map(a => `<option value="${escapeHtml(a.name)}">${escapeHtml(a.description)}</option>`).join('');
  } catch {}
}

function openModal(bottle = null) {
  loadBaseInterfaceOptions();
  const form = $('#profileForm'); form.reset(); $('#formError').textContent = '';
  $('#modalTitle').textContent = bottle ? 'Изменить профиль' : 'Новый профиль';
  form.elements.id.value = bottle?.id || '';
  form.elements.name.value = bottle?.name || '';
  form.elements.vpnExecutable.value = bottle?.vpnExecutable || '';
  form.elements.vpnArguments.value = bottle?.vpnArguments || '';
  form.elements.vpnEndpoints.value = bottle?.vpnEndpoints.join('\n') || '';
  form.elements.rdpHost.value = bottle?.rdpHost || '';
  form.elements.rdpPort.value = bottle?.rdpPort || 3389;
  form.elements.vpnInterface.value = bottle?.vpnInterface || '';
  form.elements.baseInterface.value = bottle?.baseInterface || '';
  form.elements.relayPort.value = bottle?.relayPort || 0;
  $('#profileModal').classList.add('open'); $('#profileModal').setAttribute('aria-hidden', 'false');
  setTimeout(() => form.elements.name.focus(), 50);
}
function closeModal() { $('#profileModal').classList.remove('open'); $('#profileModal').setAttribute('aria-hidden', 'true'); }

async function saveProfile(event) {
  event.preventDefault(); const form = event.currentTarget; const button = $('#saveButton');
  const data = Object.fromEntries(new FormData(form));
  const payload = { ...data, vpnEndpoints: data.vpnEndpoints.split(/\n|,/).map(x => x.trim()).filter(Boolean), rdpPort: Number(data.rdpPort), relayPort: Number(data.relayPort) };
  button.disabled = true; $('#formError').textContent = '';
  try { await api('/api/bottles', { method: 'POST', body: JSON.stringify(payload) }); closeModal(); showToast('Профиль сохранён', 'success'); await loadAll(); }
  catch (error) { $('#formError').textContent = error.message; }
  finally { button.disabled = false; }
}

async function pickVpn() {
  const button = $('#browseVpnButton'); button.disabled = true;
  try { const result = await api('/api/pick-vpn', { method: 'POST' }); $('#profileForm').elements.vpnExecutable.value = result.path; }
  catch (error) { if (!error.message.includes('не выбран')) showToast(error.message, 'error'); }
  finally { button.disabled = false; }
}

async function handleAction(action, id) {
  if (action === 'create') return openModal();
  if (action === 'start-guide') return startTour();
  if (action === 'refresh') return loadAll(true);
  const bottle = state.bottles.find(x => x.id === id); if (!bottle) return;
  if (action === 'edit') return openModal(bottle);
  if (action === 'delete') {
    if (!confirm(`Удалить профиль «${bottle.name}»?`)) return;
    try { await api(`/api/bottles/${encodeURIComponent(id)}`, { method: 'DELETE' }); showToast('Профиль удалён'); await loadAll(); } catch (error) { showToast(error.message, 'error'); }
  }
  if (action === 'start' || action === 'stop') {
    try { await api(`/api/bottles/${encodeURIComponent(id)}/${action}`, { method: 'POST' }); showToast(action === 'start' ? 'Запускаем подключение…' : 'Останавливаем сессию…', 'success'); await loadAll(); } catch (error) { showToast(error.message, 'error'); await loadAll(); }
  }
}

function switchView(name) {
  $$('.view').forEach(v => v.classList.toggle('active', v.id === `${name}View`));
  $$('.nav-item').forEach(n => n.classList.toggle('active', n.dataset.view === name));
  $('#pageName').textContent = name === 'connections' ? 'Подключения' : name === 'network' ? 'Сеть' : 'Как это работает';
  $('#topCreateButton').style.visibility = name === 'connections' ? 'visible' : 'hidden';
  $('.sidebar').classList.remove('open');
  if (name === 'network') loadNetwork();
}

const problemAnswers = {
  vpn: {
    title: 'VPN-программа не запустилась',
    steps: ['Откройте профиль кнопкой с карандашом.', 'В поле «VPN-клиент» нажмите «Выбрать…» и заново укажите установленное приложение.', 'Убедитесь, что выбрали сам файл .exe на Windows или приложение .app на macOS, а не установщик и не ярлык.', 'Сохраните профиль и попробуйте снова.']
  },
  waiting: {
    title: 'Статус «Подключение» не меняется',
    steps: ['Перейдите в открывшийся VPN-клиент и вручную нажмите подключение.', 'Дождитесь надписи Connected / Подключено в VPN-клиенте.', 'Если VPN уже подключён, откройте вкладку «Сеть» и нажмите «Обновить»: должен появиться работающий VPN-адаптер.', 'Если за две минуты ничего не произошло, нажмите «Остановить», укажите VPN-интерфейс в дополнительных параметрах и повторите запуск.']
  },
  rdp: {
    title: 'VPN подключён, но рабочий стол не открывается',
    steps: ['Проверьте RDP-адрес и порт в профиле. Стандартный порт — 3389.', 'Уточните у администратора, разрешён ли удалённый доступ к этому компьютеру и включён ли он сейчас.', 'На macOS проверьте, что установлено приложение Windows App.', 'Если окно RDP открылось, но отклоняет пароль, это уже учётные данные рабочего компьютера — Bottle их не изменяет.']
  },
  second: {
    title: 'Первый VPN работает, второй — нет',
    steps: ['Подключайте профили по очереди: дождитесь статуса «Активно» у первого и только затем запускайте второй.', 'Проверьте, что в каждом профиле указан публичный адрес именно его VPN-сервера.', 'В VPN-клиенте найдите настройку Kill Switch / Block connections without VPN: она может запрещать работу второго туннеля.', 'Если kill switch задан политикой компании и не отключается, передайте проблему администратору — одним Bottle это ограничение обойти нельзя.']
  },
  permission: {
    title: 'Почему система просит права',
    steps: ['Bottle добавляет временное правило маршрута, чтобы VPN-подключения не мешали друг другу.', 'На Windows подтвердите стандартный запрос кнопкой «Да». На macOS введите пароль пользователя в системном окне.', 'Правило не постоянное: Bottle удаляет его после остановки подключения.', 'Никогда не вводите системный пароль в веб-форму Bottle — только в настоящее окно Windows или macOS.']
  }
};

const fieldHelp = {
  name: '<b>Название</b><p>Любое понятное имя профиля. Например: «Главный офис».</p>',
  vpnExecutable: '<b>VPN-клиент</b><p>Программа, которой вы уже пользуетесь для VPN. Нажмите «Выбрать…», чтобы не вводить путь вручную.</p>',
  vpnEndpoints: '<b>Публичный адрес VPN</b><p>Адрес сервера вида vpn.company.ru. Его сообщает администратор. Это не адрес рабочего компьютера.</p>',
  rdpHost: '<b>RDP-адрес</b><p>Адрес компьютера внутри рабочей сети, часто вида 10.20.30.15.</p>',
  rdpPort: '<b>RDP-порт</b><p>Оставьте 3389, если администратор не дал другой номер.</p>'
};

const tourSteps = [
  { target: '[data-view="connections"]', eyebrow: 'Знакомство', title: 'Вкладка «Подключения»', text: 'Это главный экран. Здесь находятся сохранённые профили и кнопки запуска. Один профиль — один VPN и один удалённый компьютер.', prepare: () => { closeModal(); switchView('connections'); } },
  { target: '#createButton', eyebrow: 'Первый профиль', title: 'Создание подключения', text: 'Нажатие открывает форму настройки. Создать её нужно один раз, затем профиль запускается одной кнопкой.', prepare: () => { closeModal(); switchView('connections'); } },
  { target: '[name="name"]', eyebrow: 'Обязательное поле', title: 'Придумайте название', text: 'Введите любое понятное имя, например «Рабочий компьютер». На само подключение название не влияет.', prepare: ensureTourModal },
  { target: '[name="vpnExecutable"]', eyebrow: 'Обязательное поле', title: 'Выберите VPN-клиент', text: 'Это установленная программа, в которой вы обычно включаете VPN. Удобнее нажать «Выбрать…», чем печатать путь.', prepare: ensureTourModal },
  { target: '[name="vpnEndpoints"]', eyebrow: 'Обязательное поле', title: 'Укажите сервер VPN', text: 'Введите публичный адрес VPN от администратора. Если адресов несколько — каждый с новой строки.', prepare: ensureTourModal },
  { target: '[name="rdpHost"]', eyebrow: 'Обязательное поле', title: 'Укажите рабочий компьютер', text: 'Введите внутренний RDP-адрес. Не путайте его с адресом VPN из предыдущего шага.', prepare: ensureTourModal },
  { target: '.advanced', eyebrow: 'Можно пропустить', title: 'Дополнительные параметры', text: 'Новичку здесь обычно ничего менять не нужно. Автопоиск интерфейсов и порта уже включён.', prepare: ensureTourModal },
  { target: '[data-view="network"]', eyebrow: 'Диагностика', title: 'Вкладка «Сеть»', text: 'Она показывает сетевые адаптеры. Заходите сюда, только если Bottle не смог автоматически найти VPN.', prepare: () => { closeModal(); switchView('network'); } },
  { target: '[data-view="help"]', eyebrow: 'Всегда под рукой', title: 'Полная инструкция', text: 'Здесь можно вернуться к подготовке, расшифровке полей, порядку подключения и решениям проблем.', prepare: () => { closeModal(); switchView('help'); } }
];

function ensureTourModal() {
  switchView('connections');
  if (!$('#profileModal').classList.contains('open')) openModal();
  $('#profileModal').classList.add('tour-mode');
}

function startTour() {
  state.tourStep = 0;
  $('#tourLayer').classList.add('open');
  $('#tourLayer').setAttribute('aria-hidden', 'false');
  renderTourStep();
}

function renderTourStep() {
  $$('.tour-focus').forEach(item => item.classList.remove('tour-focus'));
  $('#profileModal').classList.remove('tour-mode');
  const step = tourSteps[state.tourStep];
  step.prepare();
  const target = $(step.target);
  if (target) {
    target.classList.add('tour-focus');
    target.scrollIntoView({ behavior: 'smooth', block: 'center' });
  }
  $('#tourStepLabel').textContent = `Шаг ${state.tourStep + 1} из ${tourSteps.length}`;
  $('#tourBar').style.width = `${((state.tourStep + 1) / tourSteps.length) * 100}%`;
  $('#tourEyebrow').textContent = step.eyebrow;
  $('#tourTitle').textContent = step.title;
  $('#tourText').textContent = step.text;
  $('#tourPrevButton').style.visibility = state.tourStep === 0 ? 'hidden' : 'visible';
  $('#tourNextButton').textContent = state.tourStep === tourSteps.length - 1 ? 'Готово' : 'Дальше';
}

function closeTour(completed = false) {
  $('#tourLayer').classList.remove('open');
  $('#tourLayer').setAttribute('aria-hidden', 'true');
  $('#profileModal').classList.remove('tour-mode');
  $$('.tour-focus').forEach(item => item.classList.remove('tour-focus'));
  if (completed) {
    try { localStorage.setItem('bottle-guide-completed', 'true'); } catch {}
    switchView('help');
    showGuideSection('quickstart');
    showToast('Тур завершён — инструкция всегда доступна здесь', 'success');
  }
}

function showGuideSection(name) {
  $$('[data-guide-panel]').forEach(panel => panel.classList.toggle('active', panel.dataset.guidePanel === name));
  $$('.guide-nav [data-guide-section]').forEach(button => button.classList.toggle('active', button.dataset.guideSection === name));
  $('.guide-content')?.scrollIntoView({ behavior: 'smooth', block: 'start' });
}

function renderProblem(name) {
  const problem = problemAnswers[name];
  $('#problemAnswer').innerHTML = `<h3>${problem.title}</h3><ol>${problem.steps.map(step => `<li>${step}</li>`).join('')}</ol>`;
  $$('[data-problem]').forEach(button => button.classList.toggle('active', button.dataset.problem === name));
}

function updateChecklist() {
  const boxes = $$('[data-check]');
  const checked = boxes.filter(box => box.checked).length;
  $('#checkProgressBar').style.width = `${checked / boxes.length * 100}%`;
  $('#checkProgressText').textContent = checked === boxes.length ? 'Всё готово — можно создавать профиль' : checked ? `Готово ${checked} из ${boxes.length}` : 'Отметьте то, что уже есть';
  boxes.forEach(box => { try { localStorage.setItem(`bottle-check-${box.dataset.check}`, box.checked ? '1' : '0'); } catch {} });
}

function showFieldHelp(button) {
  const popover = $('#fieldPopover');
  popover.innerHTML = fieldHelp[button.dataset.fieldHelp];
  popover.classList.add('open');
  const rect = button.getBoundingClientRect();
  const left = Math.min(rect.left, window.innerWidth - 290);
  popover.style.left = `${Math.max(12, left)}px`;
  popover.style.top = `${rect.bottom + 8}px`;
}

function showToast(message, type = '') { const toast = document.createElement('div'); toast.className = `toast ${type}`; toast.textContent = message; $('#toastRegion').append(toast); setTimeout(() => toast.remove(), 3800); }
function escapeHtml(value) { return String(value ?? '').replace(/[&<>'"]/g, c => ({ '&':'&amp;', '<':'&lt;', '>':'&gt;', "'":'&#39;', '"':'&quot;' }[c])); }
function plural(n, one, few, many) { const m10=n%10,m100=n%100; return m10===1&&m100!==11?one:m10>=2&&m10<=4&&(m100<12||m100>14)?few:many; }

$('#createButton').addEventListener('click', () => openModal());
$('#topCreateButton').addEventListener('click', () => openModal());
$('#refreshButton').addEventListener('click', () => loadAll(true));
$('#refreshNetworkButton').addEventListener('click', loadNetwork);
$('#closeModal').addEventListener('click', closeModal); $('#cancelModal').addEventListener('click', closeModal);
$('#profileForm').addEventListener('submit', saveProfile);
$('#browseVpnButton').addEventListener('click', pickVpn);
$('#profileModal').addEventListener('click', e => { if (e.target === e.currentTarget) closeModal(); });
document.addEventListener('keydown', e => { if (e.key === 'Escape') { $('#tourLayer').classList.contains('open') ? closeTour() : closeModal(); } });
document.addEventListener('click', e => { const action = e.target.closest('[data-action]'); if (action) handleAction(action.dataset.action, action.closest('[data-id]')?.dataset.id); });
$$('.nav-item').forEach(item => item.addEventListener('click', () => switchView(item.dataset.view)));
$('#menuButton').addEventListener('click', () => $('.sidebar').classList.toggle('open'));

$('#quickHelpButton').addEventListener('click', () => { closeModal(); switchView('help'); showGuideSection('quickstart'); });
$('#startTourButton').addEventListener('click', startTour);
$('#closeTourButton').addEventListener('click', () => closeTour());
$('#tourPrevButton').addEventListener('click', () => { if (state.tourStep > 0) { state.tourStep--; renderTourStep(); } });
$('#tourNextButton').addEventListener('click', () => { if (state.tourStep < tourSteps.length - 1) { state.tourStep++; renderTourStep(); } else closeTour(true); });
$$('[data-guide-section]').forEach(button => button.addEventListener('click', () => showGuideSection(button.dataset.guideSection)));
$$('[data-open-view]').forEach(button => button.addEventListener('click', () => switchView(button.dataset.openView)));
$$('[data-os]').forEach(button => button.addEventListener('click', () => {
  $$('[data-os]').forEach(item => item.classList.toggle('active', item === button));
  $$('[data-os-panel]').forEach(panel => panel.classList.toggle('active', panel.dataset.osPanel === button.dataset.os));
}));
$$('[data-problem]').forEach(button => button.addEventListener('click', () => renderProblem(button.dataset.problem)));
$$('[data-check]').forEach(box => {
  try { box.checked = localStorage.getItem(`bottle-check-${box.dataset.check}`) === '1'; } catch {}
  box.addEventListener('change', updateChecklist);
});
$$('[data-field-help]').forEach(button => button.addEventListener('click', event => { event.stopPropagation(); showFieldHelp(button); }));
$$('[data-field-help]').forEach(button => button.addEventListener('keydown', event => {
  if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); event.stopPropagation(); showFieldHelp(button); }
}));
document.addEventListener('click', event => { if (!event.target.closest('[data-field-help]')) $('#fieldPopover').classList.remove('open'); });
$('#copyAdminRequest').addEventListener('click', async () => {
  const text = 'Здравствуйте! Для настройки Bottle пришлите, пожалуйста:\n1. Название используемого VPN-клиента.\n2. Публичный адрес (endpoint) VPN-сервера.\n3. Внутренний RDP-адрес рабочего компьютера.\n4. RDP-порт, если он отличается от 3389.\n5. Имя VPN-интерфейса, только если автоматическое определение не работает.';
  try { await navigator.clipboard.writeText(text); showToast('Запрос скопирован', 'success'); }
  catch { showToast('Не удалось скопировать текст', 'error'); }
});
$('#exportLogsButton').addEventListener('click', async () => {
  const button = $('#exportLogsButton'); button.disabled = true;
  try {
    const result = await api('/api/logs/export', { method: 'POST' });
    showToast(`Архив создан: ${result.path}`, 'success');
  } catch (error) { showToast(error.message, 'error'); }
  finally { button.disabled = false; }
});

renderProblem('vpn');
updateChecklist();

loadAll(true);
