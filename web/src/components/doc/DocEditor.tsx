// 资料库 markdown 页面的编辑器（懒加载，见 lazy.tsx）：直接改、自动保存，Bot 改了实时合进来，每段左边标着是谁写的。
import { useEffect, useMemo, useRef, useState } from 'react'
import { EditorContent, useEditor } from '@tiptap/react'
import { api, useStore } from '../../store'
import { locale, t } from '../../i18n'
import { notice } from '../Dialog'
import Markdown from '../Markdown'
import { extensions } from './schema'
import { DocSync, REMOTE, type Conflict, type PageBlocks, type Status } from './sync'
import { ImageBox, SlashMenu, Toolbar, safeHref, slashItems } from './menus'
import { CommentsPanel, countOpen, type Draft, type PageComment } from './comments'
import { blocksOf } from './markdown'

const when = (ts: number) => new Date(ts * 1000).toLocaleString(locale, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })

export default function DocEditor({ id, fallback }: { id: number; fallback: string }) {
  const bots = useStore(s => s.bots)
  const connected = useStore(s => s.connected)
  const botsRef = useRef(bots)
  botsRef.current = bots
  const [status, setStatus] = useState<Status>('saved')
  const [conflicts, setConflicts] = useState<Conflict[]>([])
  const [editing, setEditing] = useState<string | null>(null)
  const [ready, setReady] = useState(false)
  const [err, setErr] = useState('')
  const [imageBox, setImageBox] = useState(false)
  const [comments, setComments] = useState<PageComment[]>([])
  const [panel, setPanel] = useState(false)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [focus, setFocus] = useState<string | null>(null)
  const [owner, setOwner] = useState<number | null>(null)
  const sync = useRef<DocSync | null>(null)
  const loadComments = () => api(`/api/pages/${id}/comments`).then(setComments).catch(() => {})

  const nameOf = (by: string) => {
    const m = by.match(/^bot:(\d+)$/)
    if (!m) return by === 'user' ? t('你') : t('有人')
    return botsRef.current.find(b => b.id === Number(m[1]))?.name || t('已删除的 Bot')
  }
  const exts = useMemo(() => extensions({
    who: by => {
      const m = by.match(/^bot:(\d+)$/)
      if (!m) return null
      const b = botsRef.current.find(x => x.id === Number(m[1]))
      return { name: b?.name || t('已删除的 Bot'), color: b?.avatar.color || 'var(--fg3)' }
    },
    when,
  }), [])

  // 挂上以后再建编辑器：渲染时就建的那个会在 effect 跑之前被 useEditor 销毁重建（Tiptap 的 1 毫秒延迟销毁）
  const editor = useEditor({
    extensions: exts,
    immediatelyRender: false,
    shouldRerenderOnTransaction: false,
    editorProps: {
      attributes: { class: 'md doc-body' },
      scrollThreshold: 80,
      scrollMargin: 80,
      // 链接：按住 Cmd / Ctrl 点才打开（直接点是在里面改字）；站内的页面在资料库里打开
      handleClick: (view, _pos, event) => {
        const a = (event.target as HTMLElement).closest('a')
        if (!a || (view.editable && !(event.metaKey || event.ctrlKey))) return false
        event.preventDefault()
        const href = a.getAttribute('href') || ''
        const page = href.match(/^\/library\/pages\/(\d+)$/)
        if (page) window.dispatchEvent(new CustomEvent('osa-open-page', { detail: Number(page[1]) }))
        else if (safeHref(href)) window.open(href, '_blank', 'noopener,noreferrer')
        return true
      },
      handleKeyDown: (_view, e) => {
        if ((e.metaKey || e.ctrlKey) && e.key === 's') { e.preventDefault(); sync.current?.save(); return true }   // 顺手按了保存：马上存，也不弹浏览器的另存为
        return false
      },
    },
  }, [id])

  useEffect(() => {
    if (!editor || editor.isDestroyed) return
    const s = new DocSync(id, editor)
    sync.current = s
    ;(editor.storage as any).slashCommands.items = () => slashItems(() => setImageBox(true))
    s.onChange = () => { setStatus(s.status); setConflicts([...s.conflicts.values()]) }
    s.onNotice = () => notice(t('你正在改的一段被别人删掉了，你的修改留成了新的一段。'))
    let alive = true
    let editTimer: ReturnType<typeof setTimeout> | undefined
    ;(async () => {
      if (await DocSync.recover(id) === 'lost') notice(t('上次离开时没存上的修改和别人的修改冲突了，没能补存。'))
      const data: PageBlocks & { bot_id?: number | null } = await api(`/api/pages/${id}/blocks`)
      if (!alive) return
      s.load(data)
      setOwner(data.bot_id ?? null)
      setReady(true)
      loadComments()
    })().catch(e => alive && setErr(e.message))
    const onUpdate = ({ transaction }: any) => { if (!transaction.getMeta(REMOTE)) s.schedule() }
    const onPage = (e: Event) => {
      const d = (e as CustomEvent).detail
      if (d.page_id !== id) return
      if (d.by && d.by !== 'user') {
        setEditing(d.by)
        clearTimeout(editTimer)
        editTimer = setTimeout(() => setEditing(null), 15000)
      }
      s.remote(d.version)
    }
    const onResync = () => { s.remote(Number.MAX_SAFE_INTEGER); loadComments() }   // 断线重连：漏掉的改动补上
    const onComments = (e: Event) => { if ((e as CustomEvent).detail?.page_id === id) loadComments() }
    // 光标停在有评论的段上：面板里那串评论亮起来
    const onSelect = () => {
      const at = editor.state.selection.from
      const l = blocksOf(editor.state.doc).find(b => at >= b.pos && at <= b.pos + b.node.nodeSize)
      setFocus(l?.id && s.counts.has(l.id) ? l.id : null)
    }
    editor.on('selectionUpdate', onSelect)
    window.addEventListener('osa-page-comments', onComments)
    const onHide = () => { if (document.visibilityState === 'hidden') s.save() }
    editor.on('update', onUpdate)
    window.addEventListener('osa-page', onPage)
    window.addEventListener('osa-page-resync', onResync)
    document.addEventListener('visibilitychange', onHide)
    window.addEventListener('pagehide', s.flush)
    return () => {
      alive = false
      clearTimeout(editTimer)
      editor.off('update', onUpdate)
      window.removeEventListener('osa-page', onPage)
      window.removeEventListener('osa-page-resync', onResync)
      window.removeEventListener('osa-page-comments', onComments)
      editor.off('selectionUpdate', onSelect)
      document.removeEventListener('visibilitychange', onHide)
      window.removeEventListener('pagehide', s.flush)
      s.destroy()
    }
  }, [editor, id])

  useEffect(() => { if (connected) sync.current?.retryNow() }, [connected])
  useEffect(() => {   // 每段有几串没解决的评论：画在作者条旁边
    const s = sync.current
    if (!s || !ready) return
    s.counts = countOpen(comments)
    s.paint()
  }, [comments, ready])

  // 选中文字点“评论”：挂在选区所在的那一段，选中的字作为引文
  const startComment = () => {
    if (!editor) return
    const { from, to } = editor.state.selection
    const l = blocksOf(editor.state.doc).find(b => from >= b.pos && from <= b.pos + b.node.nodeSize)
    const known = l?.id && sync.current?.base.has(l.id) ? l.id : null   // 还没存上的新段先挂整页
    setDraft({ block: known, quote: editor.state.doc.textBetween(from, to, ' ').slice(0, 200) })
    setPanel(true)
  }
  const jumpTo = (bid: string) => {
    document.querySelector(`.doc-body [data-bid="${bid}"]`)?.scrollIntoView({ block: 'center', behavior: 'smooth' })
    sync.current?.paint([bid])
    setFocus(bid)
  }
  const open = comments.filter(c => !c.parent_id && !c.resolved).length

  const STATUS: Record<Status, string> = {
    saved: t('已保存'), saving: t('保存中…'), offline: t('离线，连上以后自动保存'), conflict: t('有冲突'), error: t('保存失败'),
  }
  const jump = (bid: string) => document.querySelector(`.doc-body [data-bid="${bid}"]`)?.scrollIntoView({ block: 'center', behavior: 'smooth' })
  return (
    <div className={'doc' + (panel ? ' with-comments' : '')}>
      <div className="doc-status muted small">
        <span className={'doc-dot ' + status} />{status === 'error' && sync.current?.error ? `${STATUS.error}：${sync.current.error}` : STATUS[status]}
        {editing && <span className="doc-editing"> · {t('{name} 正在编辑', { name: nameOf(editing) })}</span>}
        <span className="grow" />
        {ready && <button className={'doc-cbtn' + (panel ? ' on' : '')} onClick={() => setPanel(!panel)} title={t('评论')}>
          💬 {open ? t('{n} 条评论', { n: open }) : t('评论')}</button>}
      </div>
      {conflicts.map(c => (
        <div className="doc-banner" key={c.id}>
          <span>{t('「{name}」也改了这一段，你的修改还没存：', { name: nameOf(c.by) })}</span>
          <button className="link-btn" onClick={() => jump(c.id)}>{t('看看是哪段')}</button>
          <button className="btn" onClick={() => sync.current?.keepMine(c.id)}>{t('用我的')}</button>
          <button className="btn" onClick={() => sync.current?.takeTheirs(c.id)}>{t('用 TA 的')}</button>
          <details className="doc-theirs"><summary>{t('TA 改成了')}</summary><Markdown text={c.md} /></details>
        </div>
      ))}
      {err && <div className="error-line">{err}</div>}
      {!ready && <div className="page-md"><Markdown text={fallback} /></div>}
      <div className="page-md doc-wrap" style={ready ? undefined : { display: 'none' }}>
        <EditorContent editor={editor} />
      </div>
      {panel && <CommentsPanel pageId={id} comments={comments} order={sync.current?.order || []} ownerBot={owner}
        blockMd={b => sync.current?.base.get(b)?.md ?? null} authorOf={b => sync.current?.base.get(b)?.by || ''}
        draft={draft} setDraft={setDraft} focus={focus} onJump={jumpTo} onClose={() => { setPanel(false); setDraft(null) }} reload={loadComments} />}
      {editor && !editor.isDestroyed && <>
        <SlashMenu editor={editor} />
        <Toolbar editor={editor} onComment={startComment} />
        {imageBox && <ImageBox editor={editor} onClose={() => setImageBox(false)} />}
      </>}
    </div>
  )
}
