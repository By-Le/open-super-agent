// App 自带的启动页。第一次打开：选用法 → 准备（测网络、起服务、装 uv 和 Python，一步一步显示）→ 准备好了。
// 以后每次打开只闪一下“正在启动”；服务没起来时显示原因。状态都从 setup_state 轮询（见 src-tauri/src/lib.rs、onboard.rs）。
const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args)

const TEXT = {
  zh: {
    chip1: '<b>Nova</b> 写好了本周的周报', chip2: '<b>Bot 的电脑</b> 正在打开浏览器', chip3: '每天 9:00 <b>整理新闻</b>',
    eyebrow: '数据留在你自己手里',
    welcome: '欢迎使用 Open Super Agent',
    welcomeLead: '你的 AI 同事，各有自己的电脑：能上网查资料、写文档、定时干活。选一种用法开始，以后可以在菜单栏里切换。',
    localBtn: '在这台电脑上运行', localHint: '推荐 · 不用装 Docker 和 Python，需要的东西会自动下载（约 35 MB）',
    haveServer: '已经有自己的服务器？连接到它', hideServer: '收起',
    remoteTitle: '连接到我的服务器', remoteBtn: '连接',
    remoteDesc: '已经在服务器上部署了 open-super-agent？填它的地址，在 App 里登录。',
    back: '取消',
    instTitle: '正在准备', instLead: '第一次打开要下载一些东西，通常不到一分钟。',
    errTitle: '下载没完成', errLead: '服务已经起来了，可以先用。运行代码和一部分应用要等这一步装好，以后用到时也会再试。',
    almost: '就快好了',
    network: '检查网络', service: '启动服务', uv: '下载 uv', python: '安装 Python 3.13',
    networkCn: '国内网络 · 从国内镜像下载', networkGlobal: '直连官方源', networkTry: '看看从哪里下载更快',
    useGlobal: '改用官方源', useCn: '改用国内镜像',
    retryGlobal: '换成官方源重试', retryCn: '换成国内镜像重试',
    serviceHint: '服务在这台电脑上运行', uvHint: '装应用要用 · 约 14 MB', pythonHint: '运行代码要用 · 约 20 MB',
    pythonRunning: '正在下载并解压…',
    left: s => `还要 ${s} 秒`, speed: '/s',
    skip: '先用着 →', retry: '重试',
    readyTitle: '准备好了', readyLead: '关掉窗口后它在菜单栏里继续运行：定时任务和聊天软件照常工作。',
    readyLocal: '运行在这台电脑上', readyCn: '运行在这台电脑上 · 国内镜像',
    openBtn: '开始使用',
    starting: '正在启动…',
    failed: '服务没能启动', failedLead: '下面是日志的最后几行。可以重试，或者换一种用法。',
    other: '换一种用法', logs: '打开日志文件夹',
  },
  en: {
    chip1: '<b>Nova</b> finished the weekly report', chip2: "<b>Bot's computer</b> is opening a browser", chip3: 'Every day 9:00 <b>news digest</b>',
    eyebrow: 'Your data stays with you',
    welcome: 'Welcome to Open Super Agent',
    welcomeLead: 'AI teammates, each with their own computer: they browse, write and work on a schedule. Pick how to run it; you can switch later from the menu bar.',
    localBtn: 'Run on this computer', localHint: 'Recommended · no Docker or Python to install (~35 MB download)',
    haveServer: 'Already have a server? Connect to it', hideServer: 'Hide',
    remoteTitle: 'Connect to my server', remoteBtn: 'Connect',
    remoteDesc: 'Already running open-super-agent on a server? Enter its address and sign in inside the app.',
    back: 'Cancel',
    instTitle: 'Getting ready', instLead: 'The first launch downloads a few things. Usually under a minute.',
    errTitle: "Download didn't finish", errLead: "The service is running, so you can start now. Running code and some apps need this step; it'll be retried when they're used.",
    almost: 'Almost there',
    network: 'Checking the network', service: 'Starting the service', uv: 'Downloading uv', python: 'Installing Python 3.13',
    networkCn: 'Mainland China · using China mirrors', networkGlobal: 'Using official sources', networkTry: 'Finding the fastest source',
    useGlobal: 'Use official sources', useCn: 'Use China mirrors',
    retryGlobal: 'Retry with official sources', retryCn: 'Retry with China mirrors',
    serviceHint: 'Running on this computer', uvHint: 'Needed to install apps · ~14 MB', pythonHint: 'Needed to run code · ~20 MB',
    pythonRunning: 'Downloading and unpacking…',
    left: s => `about ${s}s left`, speed: '/s',
    skip: 'Continue anyway →', retry: 'Try again',
    readyTitle: "You're all set", readyLead: 'Closing the window keeps it running in the menu bar, so routines and chat apps keep working.',
    readyLocal: 'Running on this computer', readyCn: 'Running on this computer · China mirrors',
    openBtn: 'Get started',
    starting: 'Starting…',
    failed: "The service didn't start", failedLead: 'Here are the last lines of the log. Try again or use it another way.',
    other: 'Use it another way', logs: 'Open logs folder',
  },
}

const $ = id => document.getElementById(id)
const SECTIONS = ['welcome', 'installing', 'ready', 'starting', 'failed']
let shown = ''
const show = which => {
  if (shown === which) return
  shown = which
  SECTIONS.forEach(id => { $(id).hidden = id !== which })
  if (which === 'ready') confetti()
}
const switching = new URLSearchParams(location.search).has('switch')
let state = null, poll = null, t = TEXT.zh, maxPct = 0, lastBytes = null, speed = 0

// 每一步占总进度的多少；正在做的那步没有字节数时，按时间慢慢往前挪（不会走到头，也不会倒退）
const WEIGHT = { network: 0.1, service: 0.1, uv: 0.4, python: 0.4 }
const CREEP = { network: 3, service: 1.5, uv: 8, python: 14 }

const mb = b => (b / 1048576).toFixed(1)

function stepDetail(s) {
  if (s.id === 'network') {
    if (s.status === 'running') return t.networkTry
    return state.mirror === 'cn' ? t.networkCn : state.mirror ? t.networkGlobal : ''
  }
  if (s.status === 'error') return s.detail
  if (s.id === 'service') return t.serviceHint
  if (s.id === 'uv') return s.status === 'running' && s.total ? `${mb(s.done)} / ${mb(s.total)} MB` : t.uvHint
  if (s.id === 'python') return s.status === 'running' ? t.pythonRunning : t.pythonHint
  return ''
}

function renderInstalling() {
  const err = state.phase === 'runtime_error'
  $('instTitle').textContent = err ? t.errTitle : t.instTitle
  $('instLead').textContent = err ? t.errLead : t.instLead
  const steps = state.steps || []
  // 步骤清单：只在条数变了时重建，其余就地改，免得动画重放
  const ol = $('steps')
  if (ol.children.length !== steps.length) {
    ol.textContent = ''
    for (const s of steps) {
      const li = document.createElement('li')
      li.innerHTML = '<span class="dot"></span><span class="st"><span class="name"></span><small></small></span><span class="secs num"></span>'
      ol.appendChild(li)
    }
  }
  let pct = 0, current = null
  steps.forEach((s, i) => {
    const li = ol.children[i]
    li.className = s.status
    li.querySelector('.name').textContent = t[s.id] || s.id
    const small = li.querySelector('small')
    small.textContent = stepDetail(s)
    // 测完网络、还没开始下载时能换下载源
    if (s.id === 'network' && s.status === 'done' && !err && state.mirror) {
      const a = document.createElement('button')
      a.className = 'link'
      a.textContent = state.mirror === 'cn' ? t.useGlobal : t.useCn
      a.onclick = () => switchMirror()
      small.appendChild(a)
    }
    li.querySelector('.secs').textContent = s.status === 'pending' || s.secs < 0.05 ? '' : fmtSecs(s.secs)
    const w = WEIGHT[s.id] || 0
    if (s.status === 'done' || s.status === 'skipped') pct += w
    else if (s.status === 'running') {
      current = current || s
      const f = s.total ? s.done / s.total : 1 - Math.exp(-s.secs / (CREEP[s.id] || 5))
      pct += w * Math.min(f, s.total ? 1 : 0.92)
    }
  })
  maxPct = Math.max(maxPct, Math.min(pct, 1))
  $('fill').style.width = (maxPct * 100).toFixed(1) + '%'
  $('pct').textContent = Math.round(maxPct * 100) + '%'
  $('bar').classList.toggle('stopped', err)
  const bad = steps.find(s => s.status === 'error')
  $('nowTitle').textContent = bad ? t[bad.id] : current ? t[current.id] : t.almost
  // 下载速度和剩下的时间
  let xfer = ''
  if (current && current.id === 'uv' && current.total) {
    const now = performance.now()
    if (lastBytes && current.done > lastBytes.b) {
      const v = (current.done - lastBytes.b) / ((now - lastBytes.t) / 1000)
      speed = speed ? speed * 0.7 + v * 0.3 : v
    }
    lastBytes = { b: current.done, t: now }
    if (speed > 0) {
      const left = Math.max(1, Math.round((current.total - current.done) / speed))
      xfer = `${mb(speed)} MB${t.speed} · ${t.left(left)}`
    }
  }
  $('xfer').textContent = xfer
  const failedStep = steps.find(s => s.status === 'error')
  $('instErr').hidden = !(err && state.message && !failedStep)
  $('instErr').textContent = state.message || ''
  $('instActions').hidden = !err
  $('otherSource').textContent = state.mirror === 'cn' ? t.retryGlobal : t.retryCn
}

const fmtSecs = s => s < 1 ? '<1s' : s < 60 ? `${Math.round(s)}s` : `${Math.floor(s / 60)}:${String(Math.round(s % 60)).padStart(2, '0')}`

async function refresh() {
  try { state = await invoke('setup_state') } catch { return }
  t = TEXT[state.zh ? 'zh' : 'en']
  document.documentElement.lang = state.zh ? 'zh-CN' : 'en'
  document.querySelectorAll('[data-t]').forEach(el => {
    const v = t[el.dataset.t]
    if (el.closest('.chip')) el.innerHTML = v   // 卡片里有 <b>，文字是这里写死的
    else el.textContent = v
  })
  $('toggleRemote').textContent = $('remoteFold').classList.contains('open') ? t.hideServer : t.haveServer
  if (state.url && !$('url').value) $('url').value = state.url
  $('backWrap').hidden = !(switching && state.mode)
  $('readyPill').textContent = state.mirror === 'cn' ? t.readyCn : t.readyLocal

  const p = state.phase
  if (p === 'installing' || p === 'runtime_error') { show('installing'); renderInstalling() }
  else if (p === 'ready') show('ready')
  else if (p === 'starting') show('starting')
  else if (p === 'error') { $('message').textContent = state.message; show('failed') }
  else if (switching || !state.mode) show('welcome')
  else show('starting')   // 正在打开服务器，马上就跳走
  clearTimeout(poll)
  if (['installing', 'starting'].includes(p) || (!p && !switching && state.mode)) poll = setTimeout(refresh, 250)
}

function startOver() { maxPct = 0; lastBytes = null; speed = 0; $('steps').textContent = '' }

async function switchMirror() {
  startOver()
  await invoke('setup_mirror', { mirror: state.mirror === 'cn' ? 'global' : 'cn' })
  refresh()
}

$('local').onclick = async () => {
  $('local').disabled = true
  startOver()
  await invoke('choose_local')
  $('local').disabled = false
  refresh()
}
$('toggleRemote').onclick = () => {
  const open = $('remoteFold').classList.toggle('open')
  $('toggleRemote').textContent = open ? t.hideServer : t.haveServer
  if (open) setTimeout(() => $('url').focus(), 200)
}
$('remote').onsubmit = async e => {
  e.preventDefault()
  const btn = e.submitter, err = $('remoteErr')
  btn.disabled = true; err.hidden = true
  try { await invoke('choose_remote', { url: $('url').value }) } catch (x) { err.textContent = String(x); err.hidden = false }
  btn.disabled = false
}
$('back').onclick = () => state.mode === 'local' ? invoke('choose_local') : invoke('choose_remote', { url: state.url })
$('retry').onclick = async () => { startOver(); await invoke('setup_retry'); refresh() }
$('retryRt').onclick = async () => { await invoke('setup_retry'); refresh() }
$('otherSource').onclick = () => switchMirror()
$('skip').onclick = () => invoke('setup_finish')
$('open').onclick = () => { $('open').disabled = true; invoke('setup_finish') }
$('other').onclick = () => show('welcome')
$('logs').onclick = () => invoke('open_logs')

// 准备好了：从两个下角撒一把彩纸（系统设了减少动态效果就不撒）
function confetti() {
  if (matchMedia('(prefers-reduced-motion: reduce)').matches) return
  const box = document.createElement('div')
  box.className = 'confetti'
  const colors = ['#d9822b', '#4c6ef5', '#2f9e44', '#e64980', '#fab005', '#868e96']
  for (let side = 0; side < 2; side++) {
    for (let i = 0; i < 40; i++) {
      const p = document.createElement('i')
      const dir = side ? -1 : 1
      p.style.left = side ? 'auto' : '-6px'
      p.style.right = side ? '-6px' : 'auto'
      p.style.background = colors[i % colors.length]
      p.style.setProperty('--dx', `${dir * (120 + Math.random() * 420)}px`)
      p.style.setProperty('--dy', `${-(260 + Math.random() * 420)}px`)
      p.style.setProperty('--r', `${Math.random() * 720 - 360}deg`)
      p.style.animationDelay = `${Math.random() * 0.25}s`
      box.appendChild(p)
    }
  }
  document.body.appendChild(box)
  setTimeout(() => box.remove(), 3200)
}

refresh()
