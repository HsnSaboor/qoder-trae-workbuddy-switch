import { useEffect, useState } from "react";

import { AuthLinkPanel } from "@/components/auth-link-panel";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import * as api from "@/lib/api";
import { useT } from "@/lib/i18n";
import type { AccountMeta, Region } from "@/lib/types";
import { useAccountsStore } from "@/stores/accounts";
import { identityDisplayName } from "@/components/account-card";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 目标版本；缺省为国内版。 */
  region?: Region;
  /** 登录采集入库成功后的回调。 */
  onSuccess?: (account: AccountMeta) => void;
}

/** OAuth 扫码登录采集：发起 → 打开浏览器 → 轮询采集结果 → 入库。 */
export function OAuthLoginDialog({ open, onOpenChange, region, onSuccess }: Props) {
  const t = useT();
  const reconcileAccounts = useAccountsStore((s) => s.reconcileAccounts);

  const [busy, setBusy] = useState(false);
  const [loginId, setLoginId] = useState<string | null>(null);
  const [uri, setUri] = useState("");
  /** 授权链接有效期（后端 `expiresIn`）—— 面板据此走倒计时。 */
  const [expiresIn, setExpiresIn] = useState<number | null>(null);
  const [error, setError] = useState("");
  const [result, setResult] = useState<AccountMeta | null>(null);

  // 打开时重置
  useEffect(() => {
    if (open) {
      setBusy(false);
      setLoginId(null);
      setUri("");
      setExpiresIn(null);
      setError("");
      setResult(null);
    }
  }, [open]);

  // 轮询采集结果
  useEffect(() => {
    if (!loginId) return;
    let timer: number | undefined;
    let cancelled = false;

    const poll = async () => {
      try {
        const res = await api.oauthStatus(loginId, region);
        if (res.done) {
          if (res.result) {
            await reconcileAccounts(region);
            if (!cancelled) {
              setResult(res.result);
              onSuccess?.(res.result);
            }
          } else if (!cancelled) {
            setError(res.error || t("wbAccounts.dialog.oauthLoginFail"));
          }
          if (timer !== undefined) window.clearInterval(timer);
          return;
        }
        timer = window.setTimeout(poll, 1500);
      } catch (e) {
        if (!cancelled) setError(api.asError(e));
        if (timer !== undefined) window.clearInterval(timer);
      }
    };
    poll();

    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [loginId, reconcileAccounts, region]);

  async function start() {
    setBusy(true);
    setError("");
    try {
      const res = await api.oauthStart(region);
      setLoginId(res.loginId);
      setUri(res.verificationUri);
      setExpiresIn(res.expiresIn ?? null);
      /* 自动开浏览器是**尽力而为**：服务器 / 容器里没有 GUI，失败不能算登录失败 ——
         面板里的链接与二维码才是完整路径（issue #62）。 */
      void openInBrowser(res.verificationUri).catch(() => {});
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t("wbAccounts.dialog.oauthTitle")}</DialogTitle>
          <DialogDescription>
            {t("wbAccounts.dialog.oauthDesc")}
          </DialogDescription>
        </DialogHeader>

        {!loginId && !result && (
          <div className="space-y-3">
            <Button onClick={start} disabled={busy} className="w-full">
              {busy ? t("wbAccounts.dialog.oauthStarting") : t("wbAccounts.dialog.oauthStart")}
            </Button>
          </div>
        )}

        {loginId && !result && (
          /* 链接 + 二维码 + 倒计时：弹窗本身就是完整路径，不依赖「自动开浏览器」。 */
          <AuthLinkPanel
            url={uri}
            expiresIn={expiresIn}
            status={error ? "expired" : "waiting"}
          >
            <p className="text-[11px] leading-relaxed text-muted-foreground">
              {t("shared.authPanel.noCallback")}
            </p>
          </AuthLinkPanel>
        )}

        {result && (
          <Alert>
            <AlertDescription>
              {t("wbAccounts.dialog.oauthCollected", { name: identityDisplayName(result) || result.id })}
            </AlertDescription>
          </Alert>
        )}

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            {t("wbAccounts.dialog.oauthClose")}
          </Button>
          {result && (
            <Button onClick={() => onOpenChange(false)}>{t("wbAccounts.dialog.oauthDone")}</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** WebUI 使用浏览器新标签页，Tauri 使用系统 opener。 */
async function openInBrowser(url: string): Promise<void> {
  if (api.isWebui()) {
    // 浏览器环境没有 Tauri 注入的 invoke；window.open 被拦截时由弹窗中的
    // 原生链接作为兜底，因此这里不把拦截视为 OAuth 失败。
    try {
      window.open(url, "_blank", "noopener,noreferrer");
    } catch {
      // 忽略自动弹窗失败；弹窗中已展示的原生链接仍可点击。
    }
    return;
  }

  const { openUrl } = await import("@tauri-apps/plugin-opener");
  return openUrl(url);
}
