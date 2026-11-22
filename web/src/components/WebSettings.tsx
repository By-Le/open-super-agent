import { useEffect, useState } from 'react'
import { api } from '../store'
import { Icon } from '../icons'
import { t } from '../i18n'
import { isEnter } from '../keys'
import { money, Section, useToast } from './settingsKit'

// 设置 → 搜索和读网页：Bot 搜索网页、读网页时按顺序用打开的几家（websearch.py），前一家不行换下一家；不用的网站
type Field = { key: string; label: string; secret: boolean; optional: boolean; placeholder: string; set: boolean; value: string }
type Service = { name: string; title: string; desc: string; link: string; missing: string; on: boolean; auto: boolean; price: number | null; provider: string; fields: Field[]
  platform: string; per_item: boolean; site_only: boolean; via: string }
type Kind = { auto: boolean; services: Service[] }
type Web = { search: Kind; fetch: Kind; posts: Kind; block: string[]; default_block: string[]; posts_budget_usd: number; usd_rate: number }
type KindName = 'search' | 'fetch' | 'posts'

export default function WebSettings() {
  const [w, setW] = useState<Web | null>(null)
  const toast = useToast()
  useEffect(() => { api<Web>('/api/web').then(setW).catch(e => toast(e.message, true)) }, [])
  const save = async (body: object, msg = t('已保存')) => {
    try { setW(await api<Web>('/api/web', { method: 'PUT', body })); toast(msg); return true }
    catch (e: any) { toast(e.message, true); return false }
  }
  if (!w) return <p className="muted small">{t('加载中…')}</p>
  const reset = (kind: KindName) => !w[kind].auto &&
    <button className="btn tiny" onClick={() => save({ [kind]: [] }, t('已改回自动'))}>{t('改回自动')}</button>
  // 按条收钱的平台真在用时才需要预算这一格
  const paid = w.posts.services.some(s => usable(s) && s.per_item && !!s.price)
  return <>
    <Section title={t('现在能做的')}>
      <Overview w={w} />
    </Section>
    <details className="m-adv web-adv">
      <summary>{t('自己挑服务、填 API Key、调顺序')}</summary>
      <Section title={t('搜索网页')} action={reset('search')}
        desc={t('Bot 搜索时按顺序用打开的几家：前一家出错、被限流或没有结果，就换下一家。{auto}', { auto: w.search.auto ? t('现在是自动：配好了的都用。') : '' })}>
        <Services kind="search" k={w.search} save={save} />
      </Section>
      <Section title={t('读网页')} action={reset('fetch')}
        desc={t('Bot 读网页正文时用。本机和内网的网页只会由这台机器自己抓，不会发给别家。{auto}', { auto: w.fetch.auto ? t('现在是自动：配好了的都用。') : '' })}>
        <Services kind="fetch" k={w.fetch} save={save} />
      </Section>
      <Section title={t('社交平台的帖子')} action={reset('posts')}
        desc={t('Bot 想知道大家在 X、Reddit、小红书上怎么说时用。同一个平台开了几家的，按顺序试。{auto}', { auto: w.posts.auto ? t('现在是自动：配好了的都用。') : '' })}>
        {paid && <Budget w={w} save={save} />}
        <Services kind="posts" k={w.posts} save={save} />
      </Section>
      <Section title={t('不用的网站')} desc={t('这些网站（含子域名）不会出现在搜索结果里，Bot 也不会读、不会在浏览器里打开。')}>
        <BlockList w={w} save={save} />
      </Section>
    </details>
  </>
}

const usable = (s: Service) => s.on && !s.missing

// 一眼看懂现在能做什么：每样事用的是哪家、要不要花钱、有没有要用户做的
function Overview({ w }: { w: Web }) {
  const line = (key: string, label: string, list: Service[], none: string) => {
    const use = list.filter(usable)
    const s = use[0]
    const browser = s?.via === 'browser'
    const cost = s && (s.price ? t('收费') : s.price === 0 ? t('免费') : '')
    const said = s && /免费|free/i.test(s.title)   // “Exa（免费）”这种名字里已经说了 i18n-ignore
    const text = !s ? none
      : (browser ? t('用电脑上的浏览器，第一次要你登录一下') : (/^[\u4e00-\u9fff]/.test(s.title) ? t('用{name}', { name: s.title }) : t('用 {name}', { name: s.title })) + (cost && !said ? ' · ' + cost : ''))
        + (use.length > 1 ? ' · ' + t('另有 {n} 家备用', { n: use.length - 1 }) : '')
    return <div key={key} className="web-ov-row">
      <Icon name={s ? 'check' : 'x'} size={15} className={s ? 'ok' : 'off'} />
      <b>{label}</b>
      <span className="muted">{text}</span>
    </div>
  }
  const plats: string[] = []
  for (const s of w.posts.services) if (s.platform && !plats.includes(s.platform)) plats.push(s.platform)
  const none = t('没有能用的，展开下面打开一家')
  const rows: [string, string, Service[], string][] = [
    ['search', t('搜索网页'), w.search.services, none],
    ['fetch', t('读网页'), w.fetch.services.filter(s => !s.via && !s.site_only), none],   // 只管某个网站、要用浏览器的不算通用的读网页
    ...plats.map((p): [string, string, Service[], string] =>
      ['p-' + p, t('{name} 上的帖子', { name: p }), w.posts.services.filter(s => s.platform === p), t('现在搜不了')]),
  ]
  // 每样事先用的那家里有收费的，就照实说钱从哪扣，不笼统说“免费”
  const paid = rows.map(r => ({ label: r[1], s: r[2].find(usable) })).filter(x => x.s?.price)
  const who = [...new Set(paid.map(x => x.s!.title))].join(t('、'))
  return <div className="role-card web-ov">
    <p className="small web-ov-note">{paid.length
      ? t('{what}在用收费服务，从你的 {who} 账户扣钱，大约每千次 {price}。不想花钱，展开下面把它关掉，会自动换成免费的。',
        { what: paid.map(x => x.label).join(t('、')), who, price: money(Math.max(...paid.map(x => x.s!.price || 0)), '¥') })
      : t('现在用的都是免费的，不用填任何东西。')}</p>
    <div className="web-ov-rows">{rows.map(r => line(...r))}</div>
  </div>
}

// 先列正在用的；没配好、默认不开的收在“还有 N 家”里，要的时候再展开
function Services({ kind, k, save }: { kind: KindName; k: Kind; save: (body: object, msg?: string) => Promise<boolean> }) {
  const toast = useToast()
  const [more, setMore] = useState(false)
  const list = k.services
  const inUse = list.filter(usable)
  const rest = list.filter(s => !usable(s))
  const onNames = (rows: Service[]) => rows.filter(s => s.on).map(s => s.name)
  const toggle = (s: Service) => {
    const next = list.map(x => x.name === s.name ? { ...x, on: !x.on } : x)
    if (!onNames(next).length) return toast(t('至少要开一家'), true)   // 关掉最后一家等于没得用
    save({ [kind]: onNames(next) }, !s.on ? t('已打开 {name}', { name: s.title }) : t('已关掉 {name}', { name: s.title }))
  }
  const move = (i: number, d: number) => {   // 只在正在用的几家之间调顺序
    const next = [...inUse]
    const [x] = next.splice(i, 1)
    next.splice(i + d, 0, x)
    save({ [kind]: onNames([...next, ...rest]) }, t('已调整顺序'))
  }
  const row = (s: Service, i: number, order: number) =>
    <ServiceRow key={s.name} kind={kind} s={s} order={order} first={i === 0} last={i === inUse.length - 1} movable={order > 0}
      onToggle={() => toggle(s)} onMove={d => move(i, d)} save={save} />
  const shown = more ? [...inUse, ...rest] : inUse
  return <>
    {shown.length > 0 && <div className="card-list">{shown.map((s, i) => row(s, i, i < inUse.length ? i + 1 : 0))}</div>}
    {!inUse.length && <p className="muted small">{t('现在一家都没在用。')}</p>}
    {rest.length > 0 && !more && <button className="btn tiny web-more" onClick={() => setMore(true)}>
      {t('还有 {n} 家可以换（有的要填 API Key）', { n: rest.length })}</button>}
  </>
}

function ServiceRow({ kind, s, order, first, last, movable, onToggle, onMove, save }: {
  kind: KindName; s: Service; order: number; first: boolean; last: boolean; movable: boolean
  onToggle: () => void; onMove: (d: number) => void; save: (body: object, msg?: string) => Promise<boolean>
}) {
  const [test, setTest] = useState<{ busy?: boolean; ok?: boolean; ms?: number; error?: string } | null>(null)
  const [open, setOpen] = useState(false)   // 填密钥、地址的输入框：点了“填 API Key”才展开
  const run = async () => {
    setTest({ busy: true })
    try { setTest(await api('/api/web/test', { method: 'POST', body: { kind, name: s.name } })) }
    catch (e: any) { setTest({ ok: false, error: e.message }) }
  }
  const f0 = s.fields[0]
  const needsField = !!s.missing && s.fields.length > 0   // 缺的是要填的值；否则是缺模型服务（下面一行说明）
  return (
    <div className="role-card">
      <div className="web-top">
        <label className="switch" title={s.on ? t('关掉') : t('打开')}>
          <input type="checkbox" checked={s.on} onChange={onToggle} /><span />
        </label>
        <div className="role-text"><b>{s.title}</b><span>{s.desc}</span></div>
        {movable && <div className="web-move">
          <button className="icon-btn" disabled={first} title={t('往前')} onClick={() => onMove(-1)}><Icon name="chevron" size={15} className="up" /></button>
          <button className="icon-btn" disabled={last} title={t('往后')} onClick={() => onMove(1)}><Icon name="chevron" size={15} /></button>
        </div>}
      </div>
      <div className="role-meta">
        {order > 0 && <span className="chip ok">{t('第 {n} 个用', { n: order })}</span>}
        {needsField && <span className="chip warn">{s.on ? t('{why}，先跳过', { why: s.missing }) : s.missing}</span>}
        {!s.on && !s.auto && <span className="chip faint">{t('默认不开')}</span>}
        {s.platform && !s.title.includes(s.platform) && <span className="chip">{s.platform}</span>}
        {s.price !== null && <span className="chip">{s.price ? (s.per_item ? t('{price} / 千条', { price: money(s.price, '¥') }) : t('{price} / 千次', { price: money(s.price, '¥') })) : t('免费')}</span>}
        {s.provider && <span className="chip faint">{t('用模型服务 {name} 的 API Key', { name: s.provider })}</span>}
        <span className="grow" />
        {test?.ok && <span className="test-ok"><Icon name="check" size={14} />{t('能用 · {s} 秒', { s: ((test.ms || 0) / 1000).toFixed(1) })}</span>}
        {f0 && <button className="btn tiny" onClick={() => setOpen(!open)}>
          {s.fields.some(f => f.set) ? t('重新填写') : t('填写')}</button>}
        {!s.missing && !s.via && <button className="btn tiny" disabled={!!test?.busy} onClick={run}><Icon name="zap" size={13} />{test?.busy ? t('测试中…') : t('测试')}</button>}
      </div>
      {s.missing && !needsField && <p className="muted small hint">{s.missing}</p>}
      {open && s.fields.map(f => <FieldInput key={f.key} s={s} f={f} save={save} />)}
      {open && s.link && s.fields.some(f => f.secret) && <a className="muted small" href={s.link} target="_blank" rel="noreferrer">{t('去 {name} 申请 API Key', { name: s.title })} <Icon name="external" size={12} /></a>}
      {test && !test.busy && !test.ok && <div className="role-error"><Icon name="alert" size={14} />{test.error}</div>}
    </div>
  )
}

function FieldInput({ s, f, save }: { s: Service; f: Field; save: (body: object, msg?: string) => Promise<boolean> }) {
  const [v, setV] = useState(f.secret ? '' : f.value)
  useEffect(() => { if (!f.secret) setV(f.value) }, [f.value])
  const dirty = f.secret ? !!v.trim() : v.trim() !== f.value
  const put = async (value: string) => {
    if (await save({ fields: { [s.name]: { [f.key]: value } } }, value ? t('已保存') : t('已删掉')) && f.secret) setV('')
  }
  const hint = f.secret ? (f.set ? t('已填（加密保存），要换就填新的') : f.optional ? t('可以不填') : '') : f.placeholder
  return (
    <div className="env-row">
      <input type={f.secret ? 'password' : 'text'} value={v} placeholder={`${f.label}${hint ? ' · ' + hint : ''}`} autoComplete="off"
        onChange={e => setV(e.target.value)} onKeyDown={e => { if (isEnter(e) && dirty) put(v.trim()) }} />
      <button className="btn" disabled={!dirty} onClick={() => put(v.trim())}>{t('保存')}</button>
      {f.secret && f.set && <button className="btn" onClick={() => put('')}>{t('删掉')}</button>}
    </div>
  )
}

// 每个任务搜按条收钱的帖子（X）最多花多少：X 一次 10 条 = 0.05 美元
function Budget({ w, save }: { w: Web; save: (body: object, msg?: string) => Promise<boolean> }) {
  const [v, setV] = useState(String(w.posts_budget_usd))
  useEffect(() => setV(String(w.posts_budget_usd)), [w.posts_budget_usd])
  const n = Number(v)
  const ok = v.trim() !== '' && !Number.isNaN(n) && n >= 0 && n <= 10
  const put = () => { if (ok && n !== w.posts_budget_usd) save({ posts_budget_usd: n }) }
  return (
    <div className="role-card web-budget">
      <div className="role-top">
        <div className="role-text"><b>{t('每个任务最多花')}</b>
          <span>{t('按条收钱的平台（X 每条 0.005 美元，一次 10 条）在一个任务里最多花这么多，用完就让 Bot 用已有结果或免费的办法。0 是不用。')}</span></div>
        <div className="web-budget-in">
          <span>$</span>
          <input type="number" min={0} max={10} step={0.05} value={v} onChange={e => setV(e.target.value)}
            onBlur={put} onKeyDown={e => { if (isEnter(e)) put() }} />
        </div>
      </div>
      {ok && <div className="role-meta"><span className="chip">{t('约 ¥{yuan}，X 最多 {n} 条', { yuan: (n * w.usd_rate).toFixed(2), n: Math.floor(n / 0.005 + 1e-9) })}</span></div>}
    </div>
  )
}

function BlockList({ w, save }: { w: Web; save: (body: object, msg?: string) => Promise<boolean> }) {
  const [v, setV] = useState('')
  const add = async () => {
    const d = v.trim()
    if (d && await save({ block: [...w.block, d] })) setV('')
  }
  const same = w.block.join() === w.default_block.join()
  return (
    <div className="role-card">
      <div className="role-meta">
        {w.block.map(d => (
          <span key={d} className="chip">{d}
            <button className="chip-x" title={t('去掉')} onClick={() => save({ block: w.block.filter(x => x !== d) })}><Icon name="x" size={12} /></button>
          </span>))}
        {!w.block.length && <span className="muted small">{t('没有')}</span>}
      </div>
      <div className="env-row">
        <input value={v} placeholder={t('域名，比如 example.com')} onChange={e => setV(e.target.value)}
          onKeyDown={e => { if (isEnter(e)) add() }} />
        <button className="btn" disabled={!v.trim()} onClick={add}>{t('添加')}</button>
        {!same && <button className="btn" onClick={() => save({ block: null }, t('已恢复'))}>{t('恢复 config.yaml 里的')}</button>}
      </div>
    </div>
  )
}
