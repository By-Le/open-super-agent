// 评论面板：按段的顺序列出评论串，点一下跳到那一段；写评论时选叫哪个 Bot（默认叫写这一段的），叫了的显示“正在处理”直到它回复。
// 评论存在服务器上（osa/pagecomments.py），这里只是界面。
import { useEffect, useMemo, useRef, useState } from 'react'
import { api, useStore } from '../../store'
import { locale, t } from '../../i18n'
import { ask } from '../Dialog'
import Markdown from '../Markdown'
import { useBack } from '../../swipe'
import { useAuthors } from '../../authors'

export type PageComment = {
  id: number; page_id: number; block_id: string | null; parent_id: number | null; by: string; text: string; quote: string
  ts: number; resolved: boolean; edited: number | null; woke: number[]
}
export type Draft = { block: string | null; quote: string }

const when = (ts: number) => new Date(ts * 1000).toLocaleString(locale, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })

/** 每段有几串没解决的评论（作者条旁边显示个数）。 */
export function countOpen(list: PageComment[]): Map<string, number> {
  const out = new Map<string, number>()
  for (const c of list) if (!c.parent_id && !c.resolved && c.block_id) out.set(c.block_id, (out.get(c.block_id) || 0) + 1)
  return out
}

function Composer({ placeholder, bots, auto, onSend, onCancel, autoFocus }: {
  placeholder: string; bots?: { id: number; name: string }[]; auto?: string; autoFocus?: boolean
  onSend: (text: string, wake: number | false | undefined) => Promise<void>; onCancel?: () => void
}) {
  const [text, setText] = useState('')
  const [wake, setWake] = useState<string>('')
  const [busy, setBusy] = useState(false)
  const send = async () => {
    if (!text.trim() || busy) return
    setBusy(true)
    try {
      await onSend(text, wake === '' ? undefined : wake === 'none' ? false : Number(wake))
      setText('')
    } finally { setBusy(false) }
  }
  return (
    <div className="dc-composer">
      <textarea value={text} autoFocus={autoFocus} rows={2} placeholder={placeholder} onChange={e => setText(e.target.value)}
        onKeyDown={e => { if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') send(); if (e.key === 'Escape') onCancel?.() }} />
      <div className="dc-row">
        {bots && <select value={wake} onChange={e => setWake(e.target.value)} title={t('谁来处理这条评论')}>
          <option value="">{auto}</option>
          {bots.map(b => <option key={b.id} value={b.id}>{t('叫 {name} 处理', { name: b.name })}</option>)}
          <option value="none">{t('不叫 Bot')}</option>
        </select>}
        <span className="grow" />
        {onCancel && <button className="link-btn" onClick={onCancel}>{t('取消')}</button>}
        <button className="btn primary small" disabled={!text.trim() || busy} onClick={send}>{t('发送')}</button>
      </div>
    </div>
  )
}

export function CommentsPanel({ pageId, comments, order, blockMd, authorOf, ownerBot, draft, setDraft, focus, onJump, onClose, reload }: {
  pageId: number; comments: PageComment[]; order: string[]; blockMd: (id: string) => string | null
  authorOf: (id: string) => string; ownerBot: number | null; draft: Draft | null; setDraft: (d: Draft | null) => void
  focus: string | null; onJump: (block: string) => void; onClose: () => void; reload: () => void
}) {
  const bots = useStore(s => s.bots)
  const root = useRef<HTMLElement>(null)
  useBack(onClose, root)   // 手机上整屏盖着页面：左边缘右滑关掉
  const [tab, setTab] = useState<'open' | 'done'>('open')
  const list = useRef<HTMLDivElement>(null)
  const name = useAuthors()
  // 按段在页面里的顺序排；整页的在最前面，原段落已经删掉的在最后
  const threads = useMemo(() => {
    const pos = new Map(order.map((id, k) => [id, k]))
    // 刚解决的（10 分钟内有人说过话）还留在“未解决”里，标着已解决：Bot 处理完顺手解决的，你还没看到它的回复
    const latest = (r: PageComment) => Math.max(...comments.filter(c => c.id === r.id || c.parent_id === r.id).map(c => c.ts))
    const fresh = (r: PageComment) => r.resolved && Date.now() / 1000 - latest(r) < 600
    const roots = comments.filter(c => !c.parent_id && (tab === 'done' ? c.resolved : !c.resolved || fresh(c)))
    const rank = (c: PageComment) => (c.block_id ? pos.get(c.block_id) ?? 1e9 : -1)
    return roots.sort((a, b) => rank(a) - rank(b) || a.id - b.id).map(r => ({ root: r, items: [r, ...comments.filter(c => c.parent_id === r.id)] }))
  }, [comments, order, tab])
  const counts = { open: comments.filter(c => !c.parent_id && !c.resolved).length, done: comments.filter(c => !c.parent_id && c.resolved).length }
  useEffect(() => { list.current?.querySelector('.dc-thread.on')?.scrollIntoView({ block: 'nearest', behavior: 'smooth' }) }, [focus])

  const post = async (body: any) => { await api(`/api/pages/${pageId}/comments`, { method: 'POST', body }); reload() }
  const patch = async (id: number, body: any) => { await api(`/api/pages/${pageId}/comments/${id}`, { method: 'PATCH', body }); reload() }
  const del = async (c: PageComment) => {
    if (!(await ask(c.parent_id ? t('删除这条评论？') : t('删除这一串评论？'), { ok: t('删除'), danger: true }))) return
    await api(`/api/pages/${pageId}/comments/${c.id}`, { method: 'DELETE' })
    reload()
  }
  const autoName = (block: string | null) => {
    const by = block ? authorOf(block) : ''
    const m = by.match(/^bot:(\d+)$/)
    const id = m ? Number(m[1]) : ownerBot
    const b = bots.find(x => x.id === id)
    return b ? t('叫 {name} 处理（写这段的）', { name: b.name }) : t('自动选 Bot')
  }
  const quoteOf = (c: PageComment) => {
    if (!c.block_id) return t('整页')
    const md = blockMd(c.block_id)
    return c.quote || (md === null ? '' : md.slice(0, 140))
  }
  return (
    <aside className="doc-comments" ref={root}>
      <header className="dc-head">
        <b>{t('评论')}</b>
        <div className="panel-tabs">
          <button className={tab === 'open' ? 'on' : ''} onClick={() => setTab('open')}>{t('未解决')} {counts.open || ''}</button>
          <button className={tab === 'done' ? 'on' : ''} onClick={() => setTab('done')}>{t('已解决')} {counts.done || ''}</button>
        </div>
        <span className="grow" />
        <button className="icon-btn" title={t('关闭')} onClick={onClose}>×</button>
      </header>
      {draft && (
        <div className="dc-thread on dc-new">
          <div className="dc-quote">{draft.quote || (draft.block ? (blockMd(draft.block) || '').slice(0, 140) : t('整页'))}</div>
          <Composer autoFocus placeholder={t('写评论；@名字 可以叫别的 Bot')} bots={bots} auto={autoName(draft.block)}
            onCancel={() => setDraft(null)}
            onSend={async (text, wake) => { await post({ text, block: draft.block, quote: draft.quote, ...(wake !== undefined ? { wake } : {}) }); setDraft(null) }} />
        </div>
      )}
      <div className="dc-list" ref={list}>
        {threads.map(({ root, items }) => {
          const gone = root.block_id !== null && blockMd(root.block_id) === null
          const lastUser = [...items].reverse().find(c => c.by === 'user' && c.woke.length)
          const answered = lastUser && items.some(c => c.ts > lastUser.ts && c.by.startsWith('bot:'))
          return (
            <div key={root.id} className={'dc-thread' + (focus && root.block_id === focus ? ' on' : '') + (root.resolved ? ' done' : '')}
              onClick={() => root.block_id && !gone && onJump(root.block_id)}>
              <div className={'dc-quote' + (gone ? ' gone' : '')}>{gone ? t('原段落已删除') : quoteOf(root)}</div>
              {root.resolved && tab === 'open' && <div className="dc-done small">✓ {t('已解决')}</div>}
              {items.map(c => (
                <div key={c.id} className="dc-item">
                  <div className="dc-meta"><b>{name(c.by)}</b> <span className="muted">{when(c.ts)}</span>
                    {c.by === 'user' && <button className="dc-del" title={t('删除')} onClick={e => { e.stopPropagation(); del(c) }}>×</button>}</div>
                  <div className="dc-text"><Markdown text={c.text} /></div>
                </div>
              ))}
              {lastUser && !answered && !root.resolved && <div className="dc-pending muted small">
                {t('已叫 {names}，正在处理…', { names: lastUser.woke.map(id => name(`bot:${id}`)).join('、') })}</div>}
              <div className="dc-foot" onClick={e => e.stopPropagation()}>
                <Composer placeholder={t('回复…')} onSend={text => post({ text, reply_to: root.id })} />
                <button className="link-btn small" onClick={() => patch(root.id, { resolved: !root.resolved })}>
                  {root.resolved ? t('重新打开') : t('标成已解决')}</button>
              </div>
            </div>
          )
        })}
        {!threads.length && !draft && <p className="muted small dc-empty">
          {tab === 'open' ? t('没有没解决的评论。选中一段文字点“评论”，Bot 会来处理；也可以对整页提意见。') : t('还没有解决过的评论。')}</p>}
      </div>
      {!draft && tab === 'open' && <button className="btn dc-page" onClick={() => setDraft({ block: null, quote: '' })}>{t('评论整页')}</button>}
    </aside>
  )
}
