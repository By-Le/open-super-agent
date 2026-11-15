// 设置 → 外部 Agent：给 Claude Code、Codex 这些 OSA 外面的 Agent 建 token，让它们经 /mcp 读写资料库（osa/agentapi.py）。
// token 只在建好时显示一次（服务器只存 sha256），这里顺手给出各家的接法，复制就能用。撤销后马上连不上，它写过的内容和署名都留着。
import { useEffect, useState } from 'react'
import { api, useStore } from '../store'
import { locale, t, tn } from '../i18n'
import { ask } from './Dialog'
import type { AgentToken } from '../types'

const when = (ts: number) => new Date(ts * 1000).toLocaleString(locale, { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })
const scopeName = (s: string) => (s === 'write' ? t('能读能写') : t('只读'))

function Copy({ text }: { text: string }) {
  const [ok, setOk] = useState(false)
  return <button className="ghost-btn" onClick={() => { navigator.clipboard?.writeText(text); setOk(true); setTimeout(() => setOk(false), 1500) }}>
    {ok ? t('已复制') : t('复制')}</button>
}

function Snippet({ text }: { text: string }) {
  return <div className="agent-snippet"><pre>{text}</pre><Copy text={text} /></div>
}

// 刚建好的 token 和各家的接法：关掉就再也看不到 token 了
function Made({ made, url, onClose }: { made: AgentToken & { token: string }; url: string; onClose: () => void }) {
  const claude = `claude mcp add -s user --transport http osa ${url} --header "Authorization: Bearer ${made.token}"`
  const codex = `[mcp_servers.osa]\nurl = "${url}"\nbearer_token_env_var = "OSA_TOKEN"`
  return (
    <div className="agent-made">
      <p><b>{t('「{name}」的 token 建好了。只显示这一次，关掉就看不到了，先复制下来。', { name: made.name })}</b></p>
      <Snippet text={made.token} />
      <h4>Claude Code</h4>
      <p className="muted small">{t('在终端里运行（-s user 是每个项目都能用；去掉就只在当前目录的项目里）：')}</p>
      <Snippet text={claude} />
      <h4>Codex</h4>
      <p className="muted small">{tn('加到 {file}，再在启动 Codex 的环境里设好 {env}：', { file: <code>~/.codex/config.toml</code>, env: <code>OSA_TOKEN</code> })}</p>
      <Snippet text={codex} />
      <Snippet text={`export OSA_TOKEN=${made.token}`} />
      <h4>{t('其他客户端')}</h4>
      <p className="muted small">{tn('支持 Streamable HTTP 的 MCP 客户端都能接：地址填 {url}，请求头带 {header}。', {
        url: <code>{url}</code>, header: <code>Authorization: Bearer &lt;token&gt;</code> })}</p>
      <div className="row-actions"><button className="btn" onClick={onClose}>{t('复制好了，关掉')}</button></div>
    </div>
  )
}

export default function AgentTokens() {
  const demo = useStore(s => s.settings?.security?.demo)
  const [list, setList] = useState<AgentToken[] | null>(null)
  const [name, setName] = useState('')
  const [scope, setScope] = useState<'read' | 'write'>('write')
  const [made, setMade] = useState<(AgentToken & { token: string }) | null>(null)
  const [err, setErr] = useState('')
  const load = () => api<AgentToken[]>('/api/agent-tokens').then(setList).catch((e: any) => setErr(e.message))
  useEffect(() => { if (!demo) load() }, [demo])
  const url = location.origin + '/mcp'
  if (demo) return <p className="muted">{t('演示服务器不开放外部 Agent。')}</p>
  const add = async () => {
    setErr('')
    try {
      setMade(await api('/api/agent-tokens', { method: 'POST', body: { name, scope } }))
      setName('')
      load()
    } catch (e: any) { setErr(e.message) }
  }
  const revoke = async (x: AgentToken) => {
    if (!await ask(t('撤销「{name}」？撤销后它马上就连不上了；它写过的内容和署名都留着。', { name: x.name }), { ok: t('撤销'), danger: true })) return
    try { await api(`/api/agent-tokens/${x.id}`, { method: 'DELETE' }); load() } catch (e: any) { setErr(e.message) }
  }
  return (
    <div>
      <p className="muted">{tn('它们写的段署它们的名字，你在网页上实时看到；它们写的评论不会叫醒 Bot；你锁定的页面它们只能读。每个 Agent 建一个 token，不用了就撤销。MCP 地址：{url}', { url: <code>{url}</code> })}</p>
      {made ? <Made made={made} url={url} onClose={() => setMade(null)} /> : <>
        <h4>{t('新建')}</h4>
        <div className="env-row">
          <input value={name} maxLength={40} onChange={e => setName(e.target.value)} onKeyDown={e => { if (e.key === 'Enter' && name.trim()) add() }}
            placeholder={t('名字，比如 Claude Code')} />
          <select value={scope} onChange={e => setScope(e.target.value as 'read' | 'write')}>
            <option value="write">{scopeName('write')}</option>
            <option value="read">{scopeName('read')}</option>
          </select>
          <button className="btn primary" disabled={!name.trim()} onClick={add}>{t('新建')}</button>
        </div>
      </>}
      {err && <div className="error-line">{err}</div>}
      <h4>{t('已建的')}</h4>
      {list?.map(x => (
        <div className={'kv agent-row' + (x.revoked ? ' revoked' : '')} key={x.id}>
          <span><b>{x.name}</b> <span className="chip">{scopeName(x.scope)}</span></span>
          <span className="agent-row-side">
            <span className="muted small">{x.revoked ? t('{time} 撤销了', { time: when(x.revoked) })
              : x.last_used ? t('最后用过：{time}', { time: when(x.last_used) }) : t('还没用过')}</span>
            {!x.revoked && <button className="ghost-btn" onClick={() => revoke(x)}>{t('撤销')}</button>}
          </span>
        </div>))}
      {list && !list.length && <div className="muted small">{t('还没有外部 Agent')}</div>}
    </div>
  )
}
