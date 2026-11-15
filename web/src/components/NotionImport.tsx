// 从 Notion 导入（osa/notion_import.py）：用应用市场里连着的 Notion 搜页面和数据库，勾上要的导进资料库（连子页面、库里的行）。
// 导过的再导一次是更新：页面正文按段对齐，行的属性换成 Notion 现在的。导入在服务器后台跑，这里每秒看一次进度。
import { useEffect, useRef, useState } from 'react'
import { api, openLibrary } from '../store'
import { locale, t } from '../i18n'
import { useBack } from '../swipe'

type Hit = { id: string; title: string; type: string; path?: string }
type Done = { nid: string; page_id: number; title: string; kind: string; synced: number }
type Job = { state: 'running' | 'done' | 'failed'; step: string; counts: Record<string, number>; log: string[]; error: string
  roots: { nid: string; page_id: number | null }[] }

const when = (ts: number) => new Date(ts * 1000).toLocaleString(locale, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })
const clean = (id: string) => id.replace(/-/g, '')

export default function NotionImport({ onClose }: { onClose: () => void }) {
  const root = useRef<HTMLDivElement>(null)
  useBack(onClose, root)
  const [q, setQ] = useState('')
  const [hits, setHits] = useState<Hit[] | null>(null)
  const [done, setDone] = useState<Done[]>([])
  const [pick, setPick] = useState<Map<string, string>>(new Map())   // Notion 编号 → 标题
  const [job, setJob] = useState<number | null>(null)
  const [state, setState] = useState<Job | null>(null)
  const [err, setErr] = useState('')
  useEffect(() => { api<Done[]>('/api/notion/imported').then(setDone).catch(() => {}) }, [])
  useEffect(() => {   // 打字停一下再搜
    if (!q.trim()) { setHits(null); return }
    const h = setTimeout(async () => {
      try {
        const r = await api<{ text: string }>('/api/notion/read', { method: 'POST', body: { tool: 'notion-search', args: { query: q.trim() } } })
        setHits((JSON.parse(r.text).results || []).filter((x: Hit) => x.type === 'page' || x.type === 'database'))
        setErr('')
      } catch (e: any) { setErr(e.message); setHits([]) }
    }, 400)
    return () => clearTimeout(h)
  }, [q])
  useEffect(() => {
    if (job === null) return
    const h = setInterval(async () => {
      try {
        const s = await api<Job>(`/api/notion/import/${job}`)
        setState(s)
        if (s.state !== 'running') clearInterval(h)
      } catch { clearInterval(h) }
    }, 1000)
    return () => clearInterval(h)
  }, [job])
  const toggle = (id: string, title: string) => {
    const next = new Map(pick)
    if (next.has(id)) next.delete(id); else next.set(id, title)
    setPick(next)
  }
  const go = async (roots: string[]) => {
    setErr('')
    try { setJob((await api<{ job: number }>('/api/notion/import', { method: 'POST', body: { roots } })).job) } catch (e: any) { setErr(e.message) }
  }
  const STEP: Record<string, string> = { crawl: t('读页面…'), rows: t('读数据库的行…'), create: t('建页面和数据库…'), content: t('写正文和行…'), relations: t('对上关联…') }
  return (
    <div className="modal-bg" ref={root} onMouseDown={() => state?.state !== 'running' && onClose()}>
      <div className="modal notion-import" onMouseDown={e => e.stopPropagation()}>
        <h3>{t('从 Notion 导入')}</h3>
        {job === null ? <>
          <p className="muted small">{t('用应用市场里连着的 Notion 读，只读不写。勾上的页面连同它下面的子页面、数据库（连每一行）一起导进来，Notion 里的链接换成资料库里的。导过的再导一次是更新。')}</p>
          <input autoFocus value={q} placeholder={t('在 Notion 里搜页面或数据库')} onChange={e => setQ(e.target.value)} />
          <div className="move-list ni-list">
            {hits === null ? <>
              {!!done.length && <div className="muted small ni-head">{t('导入过的（勾上再导一次就是更新）')}</div>}
              {done.map(d => (
                <label key={d.nid} className="ni-row"><input type="checkbox" checked={pick.has(d.nid)} onChange={() => toggle(d.nid, d.title)} />
                  <span>{d.kind === 'db' ? '▦' : '▤'} {d.title}</span><span className="muted small">{t('{time} 导入', { time: when(d.synced) })}</span></label>))}
              {!done.length && <p className="muted small">{t('搜一下要导入的页面，比如公司、项目的总页面。')}</p>}
            </> : hits.map(h => (
              <label key={h.id} className="ni-row"><input type="checkbox" checked={pick.has(clean(h.id))} onChange={() => toggle(clean(h.id), h.title)} />
                <span>{h.type === 'database' ? '▦' : '▤'} {h.title}</span>{h.path && <span className="muted small">{h.path}</span>}</label>))}
            {hits !== null && !hits.length && <p className="muted small">{t('没搜到')}</p>}
          </div>
          {!!pick.size && <p className="small">{t('要导入：{names}', { names: [...pick.values()].map(x => `「${x}」`).join(t('、')) })}</p>}
        </> : <div className="ni-progress">
          {!state || state.state === 'running'
            ? <p><span className="spin" /> {STEP[state?.step || 'crawl'] || t('导入中…')}</p>
            : state.state === 'done' ? <p>✓ {t('导完了')}</p> : <p className="error-line">{t('没导完：{error}', { error: state.error })}</p>}
          {state && <p className="muted small">{t('新建 {pages} 个页面、{dbs} 个数据库、{rows} 行，更新 {updated} 处，下载 {files} 个文件', {
            pages: state.counts.pages, dbs: state.counts.dbs, rows: state.counts.rows, updated: state.counts.updated, files: state.counts.files })}</p>}
          {!!state?.log.length && <details><summary className="small">{t('{n} 条说明', { n: state.log.length })}</summary>
            <ul className="small">{state.log.map((l, k) => <li key={k}>{l}</li>)}</ul></details>}
          {state?.state === 'done' && state.roots.filter(r => r.page_id).map(r => (
            <button key={r.nid} className="link-btn" onClick={() => { onClose(); openLibrary(r.page_id!) }}>{t('打开「{title}」', { title: pick.get(r.nid) || r.nid })}</button>))}
        </div>}
        {err && <div className="error-line">{err}</div>}
        <div className="modal-actions">
          {job === null && <button className="btn primary" disabled={!pick.size} onClick={() => go([...pick.keys()])}>{t('导入')}</button>}
          <button className="btn" disabled={state?.state === 'running'} onClick={onClose}>{job === null ? t('取消') : t('关闭')}</button>
        </div>
      </div>
    </div>
  )
}
