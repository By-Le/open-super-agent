// 资料库的页面树：子页面挂在上一级下面（parent_id），同一层按 pos 排；最上层还是置顶的在前、新的在前。
// 也放“挪到哪里”的对话框：不能挪到自己和自己的子页面下面，html 页面下面不放子页面。
import { useMemo, useRef, useState } from 'react'
import { api, openLibrary } from '../store'
import type { Page } from '../types'
import { t } from '../i18n'
import { ask } from './Dialog'
import { useBack } from '../swipe'

export type Node = { page: Page; kids: Node[]; depth: number }

export function buildTree(list: Page[]): Node[] {
  const ids = new Set(list.map(p => p.id))
  const kids = new Map<number, Page[]>()
  for (const p of list) if (p.parent_id != null && ids.has(p.parent_id)) kids.set(p.parent_id, [...(kids.get(p.parent_id) || []), p])
  const grow = (p: Page, depth: number, seen: Set<number>): Node => ({
    page: p, depth,
    kids: seen.has(p.id) ? [] : (kids.get(p.id) || []).sort((a, b) => a.pos - b.pos || a.id - b.id).map(k => grow(k, depth + 1, new Set([...seen, p.id]))),
  })
  return list.filter(p => p.parent_id == null || !ids.has(p.parent_id)).map(p => grow(p, 0, new Set()))
}

const KEY = 'osa.libtree'
function useExpanded(): [Set<number>, (id: number) => void] {
  const [open, setOpen] = useState<Set<number>>(() => {
    try { return new Set(JSON.parse(localStorage.getItem(KEY) || '[]')) } catch { return new Set() }
  })
  const toggle = (id: number) => setOpen(cur => {
    const next = new Set(cur)
    if (!next.delete(id)) next.add(id)
    try { localStorage.setItem(KEY, JSON.stringify([...next])) } catch { /* 存不了就算了 */ }
    return next
  })
  return [open, toggle]
}

export async function newPage(parent: number | null = null) {
  const p = await api('/api/pages', { method: 'POST', body: { title: t('无标题'), content: '', allow_empty: true, ...(parent ? { parent_id: parent } : {}) } })
  openLibrary(p.id)
}

export async function deletePage(p: Page, kids: number) {
  const msg = kids ? t('删除页面「{title}」？它下面的 {n} 个子页面会挪到上一级，分享链接也会失效。', { title: p.title, n: kids })
    : t('删除页面「{title}」？分享链接也会失效。', { title: p.title })
  if (!(await ask(msg, { ok: t('删除'), danger: true }))) return false
  await api(`/api/pages/${p.id}`, { method: 'DELETE' })
  return true
}

/** 页面列表（树）。row 画一行的内容（图标、标题、作者这些沿用资料库原来的样子）。 */
export function PageTree({ list, row, onMove }: { list: Page[]; row: (p: Page) => React.ReactNode; onMove: (p: Page) => void }) {
  const tree = useMemo(() => buildTree(list), [list])
  const [open, toggle] = useExpanded()
  const [menu, setMenu] = useState<number | null>(null)
  const draw = (n: Node): React.ReactNode => (
    <div key={n.page.id}>
      <div className="tree-row" style={{ paddingLeft: n.depth * 18 }}>
        <button className={'tree-chev' + (n.kids.length ? '' : ' none')} onClick={() => n.kids.length && toggle(n.page.id)}
          title={n.kids.length ? t('展开或收起子页面') : undefined}>{n.kids.length ? (open.has(n.page.id) ? '▾' : '▸') : ''}</button>
        <button className="lib-row" onClick={() => openLibrary(n.page.id)}>{row(n.page)}</button>
        <div className="tree-acts">
          {n.page.format === 'markdown' && <button className="icon-btn" title={t('新建子页面')} onClick={() => newPage(n.page.id)}>＋</button>}{/* i18n-ignore */}
          <div className="page-more">
            <button className="icon-btn" title={t('更多')} onClick={() => setMenu(menu === n.page.id ? null : n.page.id)}>⋯</button>
            {menu === n.page.id && <div className="menu" onMouseLeave={() => setMenu(null)}>
              <button onClick={() => { setMenu(null); onMove(n.page) }}>{t('挪到…')}</button>
              <button className="danger-text" onClick={async () => { setMenu(null); await deletePage(n.page, n.kids.length) }}>{t('删除')}</button>
            </div>}
          </div>
        </div>
      </div>
      {n.kids.length > 0 && open.has(n.page.id) && n.kids.map(draw)}
    </div>
  )
  return <>{tree.map(draw)}</>
}

/** 挪到哪里：列出能放的页面（缩进成树），也可以挪到最上层。 */
export function MoveDialog({ page, list, onClose }: { page: Page; list: Page[]; onClose: () => void }) {
  const root = useRef<HTMLDivElement>(null)
  useBack(onClose, root)
  const [q, setQ] = useState('')
  const [err, setErr] = useState('')
  const banned = useMemo(() => {   // 自己和自己的子孙
    const out = new Set([page.id])
    let grew = true
    while (grew) {
      grew = false
      for (const p of list) if (p.parent_id != null && out.has(p.parent_id) && !out.has(p.id)) { out.add(p.id); grew = true }
    }
    return out
  }, [page, list])
  const flat: Node[] = []
  const walk = (n: Node) => { flat.push(n); n.kids.forEach(walk) }
  buildTree(list).forEach(walk)
  const shown = flat.filter(n => !banned.has(n.page.id) && n.page.format === 'markdown' && (!q || n.page.title.toLowerCase().includes(q.toLowerCase())))
  const go = async (parent: number | null) => {
    try {
      await api(`/api/pages/${page.id}/move`, { method: 'POST', body: { parent_id: parent } })
      onClose()
    } catch (e: any) { setErr(e.message) }
  }
  return (
    <div className="modal-bg" ref={root} onMouseDown={onClose}>
      <div className="modal small move-dialog" onMouseDown={e => e.stopPropagation()}>
        <h3>{t('把「{title}」挪到…', { title: page.title })}</h3>
        <input autoFocus value={q} placeholder={t('按标题找')} onChange={e => setQ(e.target.value)} />
        <div className="move-list">
          {!q && page.parent_id != null && <button onClick={() => go(null)}>{t('最上层')}</button>}
          {shown.map(n => (
            <button key={n.page.id} style={{ paddingLeft: 10 + (q ? 0 : n.depth * 16) }} disabled={n.page.id === page.parent_id}
              onClick={() => go(n.page.id)}>{n.page.title}{n.page.id === page.parent_id ? ' · ' + t('现在在这里') : ''}</button>
          ))}
          {!shown.length && <p className="muted small">{t('没有能放的页面')}</p>}
        </div>
        {err && <div className="error-line">{err}</div>}
        <div className="modal-actions"><button className="btn" onClick={onClose}>{t('取消')}</button></div>
      </div>
    </div>
  )
}
