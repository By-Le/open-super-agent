// 资料库里的文档：交给 Open Agent Docs（见 docs/plans/oad.md）。浏览器只连本站：/api/v1/* 由 osa-server 带上用户转过去。
// 这个文件按需加载（编辑器、Yjs 比较大），不进首屏的包。
import { createClient, type Me, type Page } from '@open-agent-docs/client'
import {
  OadComments, OadDatabaseView, OadEditor, OadProvider, OadTitle, colorFor, focusStart, setLocale as setOadLocale,
  useOadPage, usePageComments, usePeople, type CommentAnchor, type Editor,
} from '@open-agent-docs/editor'
import '@open-agent-docs/editor/style.css'
import { useEffect, useMemo, useState } from 'react'
import { openLibrary } from '../store'
import { lang, locale, t } from '../i18n'
import { ask } from './Dialog'

const client = createClient({ baseUrl: '', locale: lang })
setOadLocale(lang)

const when = (ms: number) => new Date(ms).toLocaleString(locale, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })
// 桌面 App 在这台电脑上运行时，地址是 127.0.0.1：分享出去的链接别人打不开
const localOnly = ['127.0.0.1', 'localhost', '[::1]'].includes(location.hostname) || location.protocol === 'tauri:'

let meP: Promise<Me> | null = null
const getMe = () => (meP ??= client.me().catch(e => { meP = null; throw e }))

function useMe() {
  const [me, setMe] = useState<Me | null>(null)
  const [err, setErr] = useState('')
  useEffect(() => { getMe().then(setMe).catch((e: any) => setErr(e.message || String(e))) }, [])
  return { me, err }
}

// 页面按目录排：父页面下面跟着子页面
function ordered(pages: Page[]) {
  const kids = new Map<string | null, Page[]>()
  for (const p of pages) {
    if (p.kind === 'row') continue
    const k = p.parent_id ?? null
    kids.set(k, [...(kids.get(k) || []), p])
  }
  const out: { p: Page; depth: number }[] = []
  const walk = (k: string | null, depth: number) => {
    for (const p of (kids.get(k) || []).sort((a, b) => (a.position < b.position ? -1 : 1))) {
      out.push({ p, depth })
      walk(p.id, depth + 1)
    }
  }
  walk(null, 0)
  return out
}

export function DocList() {
  const { me, err } = useMe()
  const [pages, setPages] = useState<Page[] | null>(null)
  const ws = me?.workspaces[0]?.id
  useEffect(() => {
    if (!ws) return
    const load = () => client.pages(ws).then(setPages).catch(() => setPages([]))
    load()
    const ev = client.events(ws)
    const upsert = (p: Page) => setPages(l => (l ? (l.some(x => x.id === p.id) ? l.map(x => (x.id === p.id ? { ...x, ...p } : x)) : [...l, p]) : l))
    ev.on('page.created', upsert)
    ev.on('page.updated', upsert)
    ev.on('page.deleted', (p: { id: string }) => setPages(l => l && l.filter(x => x.id !== p.id)))
    ev.on('resync', load)
    return () => ev.close()
  }, [ws])
  if (err) return <div className="error-line">{t('文档服务连不上：{error}', { error: err })}</div>
  if (!pages) return <div className="muted">{t('加载中…')}</div>
  const create = async () => {
    const p = await client.createPage(ws!, { title: '' })
    openLibrary(p.id)
  }
  const rows = ordered(pages)
  return (
    <div className="lib-list">
      <p className="lib-intro muted small">{t('文档是你和 Bot 一起写的：直接在里面编辑，选中一段写评论、@ 某个 Bot，它就会来改、来回复。可以分享给别人看。')}</p>
      <div className="doc-new"><button className="btn primary" onClick={create}>+ {t('新建文档')}</button></div>
      {!rows.length && <div className="lib-empty"><p className="muted small">{t('还没有文档。新建一篇，或者让 Bot 把结果写成文档。')}</p></div>}
      {rows.map(({ p, depth }) => (
        <button key={p.id} className="lib-row" style={{ paddingLeft: 10 + depth * 18 }} onClick={() => openLibrary(p.id)}>
          <span className="lib-icon">{p.icon || (p.kind === 'database' ? '▦' : '▤')}</span>
          <span className="lib-main">
            <b>{p.title || t('无标题')}</b>
            <span className="lib-meta">{when(p.updated_at)}</span>
          </span>
        </button>
      ))}
    </div>
  )
}

export function DocView({ id }: { id: string }) {
  const { me, err } = useMe()
  const [page, setPage] = useState<Page | null>(null)
  const [missing, setMissing] = useState('')
  useEffect(() => {
    client.page(id).then(setPage).catch((e: any) => setMissing(e.message || String(e)))
  }, [id])
  const ws = me?.workspaces[0]?.id
  return (
    <OadProvider client={client}>
      <header className="chat-head page-head">
        <button className="ghost-btn back" onClick={() => openLibrary()}>← {t('资料库')}</button>
        <span className="head-name page-title">{page ? page.title || t('无标题') : ''}</span>
        <span className="grow" />
        {page && <DocActions page={page} />}
      </header>
      {err || missing ? <div className="lib-body"><div className="error-line">{err || missing}</div></div>
        : !me || !page || !ws ? <div className="lib-body muted">{t('加载中…')}</div>
        : page.kind === 'database' ? <DbBody page={page} me={me} ws={ws} />
        : <DocBody key={page.id} page={page} me={me} ws={ws} onTitle={title => setPage(p => (p ? { ...p, title } : p))} />}
    </OadProvider>
  )
}

function DocActions({ page }: { page: Page }) {
  const [share, setShare] = useState<{ token: string } | null | undefined>(undefined)
  const [copied, setCopied] = useState(false)
  useEffect(() => { client.getShare(page.id).then(setShare).catch(() => setShare(null)) }, [page.id])
  const url = (tok: string) => `${location.origin}/s/${tok}`
  const copy = (tok: string) => { navigator.clipboard?.writeText(url(tok)); setCopied(true); setTimeout(() => setCopied(false), 1500) }
  const on = async () => {
    if (localOnly && !(await ask(t('现在是在这台电脑上运行，分享链接只有这台电脑能打开。要让别人也能看，用连到服务器的方式。仍然生成链接？'), { ok: t('生成') }))) return
    const s = await client.createShare(page.id, {})
    setShare(s)
    copy(s.token)
  }
  const off = async () => { await client.revokeShare(page.id); setShare(null) }
  const del = async () => {
    if (!(await ask(t('把文档「{title}」移到回收站？分享链接也会失效。', { title: page.title || t('无标题') }), { ok: t('删除'), danger: true }))) return
    await client.deletePage(page.id)
    openLibrary()
  }
  return (
    <div className="page-actions">
      {share === undefined ? null : share
        ? <><button className="btn" onClick={() => copy(share.token)}>{copied ? t('已复制链接') : t('复制链接')}</button>
            <button className="btn" onClick={off}>{t('取消分享')}</button></>
        : <button className="btn" onClick={on}>{t('分享')}</button>}
      <button className="btn danger" onClick={del}>{t('删除')}</button>
    </div>
  )
}

function DbBody({ page, me, ws }: { page: Page; me: Me; ws: string }) {
  const events = useMemo(() => client.events(ws), [ws])
  useEffect(() => () => events.close(), [events])
  return (
    <div className="scroller"><div className="doc-db">
      <OadDatabaseView dbId={page.id} me={me} ws={ws} canWrite events={events} embedded navigate={(pid: string) => openLibrary(pid)} />
    </div></div>
  )
}

function DocBody({ page, me, ws, onTitle }: { page: Page; me: Me; ws: string; onTitle: (t: string) => void }) {
  const user = useMemo(() => ({ id: me.user.id, name: me.user.name, color: colorFor(me.user.id) }), [me])
  const doc = useOadPage(client, page.id, { user })
  const events = useMemo(() => client.events(ws), [ws])
  useEffect(() => () => events.close(), [events])
  const { threads, reload } = usePageComments(client, page.id, events)
  const people = usePeople(client, ws, me, {}, threads)
  const [draft, setDraft] = useState<CommentAnchor | null>(null)
  const [active, setActive] = useState<string | null>(null)
  const [editor, setEditor] = useState<Editor | null>(null)
  if (!doc.ydoc) return <div className="lib-body muted">{t('连接中…')}</div>
  return (
    <div className="doc-split">
      <div className="scroller">
        <article className="doc-main" data-status={doc.status}>
          <OadTitle ydoc={doc.ydoc} placeholder={t('无标题')} onEnter={() => editor && focusStart(editor)} onChange={onTitle} />
          <OadEditor ydoc={doc.ydoc} provider={doc.provider} user={user} onReady={setEditor}
            onComment={a => { setDraft(a); setActive(null) }} />
        </article>
      </div>
      <aside className="doc-comments">
        <OadComments pageId={page.id} me={me} editor={doc.ready ? editor : null} threads={threads} reload={reload}
          active={active} setActive={setActive} draft={draft} setDraft={setDraft}
          candidates={people.candidates} nameOf={people.nameOf} canWrite docVersion={0} onClose={() => setDraft(null)} />
      </aside>
    </div>
  )
}
