import { useCallback, useState } from "react";
import { RefreshCw, Upload } from "lucide-react";
import { useTranslation } from "react-i18next";
import {
  DataTransferImportResult,
  importDataTransferJson,
} from "../services/dataTransferService";
import {
  createManagedBackup,
  getAutoBackupSettings,
  getSelectionFromAutoBackupSettings,
} from "../services/scheduledBackupService";
import {
  getWebdavSyncSettings,
  listWebdavBackupFiles,
  readWebdavBackupFile,
  uploadAutoBackupToWebdav,
} from "../services/webdavSyncService";
import { useCursorAccountStore } from "../stores/useCursorAccountStore";

type FeedbackTone = "loading" | "success" | "error";

interface HeaderFeedback {
  tone: FeedbackTone;
  text: string;
}

function normalizeError(error: unknown): string {
  return String(error).replace(/^Error:\s*/, "");
}

function buildImportFeedback(
  result: DataTransferImportResult,
  t: (key: string, options?: Record<string, unknown>) => string,
): string {
  const parts: string[] = [];
  if (result.imported_account_count > 0) {
    parts.push(
      t("settings.transfer.feedback.accountsImported", {
        count: result.imported_account_count,
        defaultValue: "已导入 {{count}} 个账号",
      }),
    );
  }
  if (result.config_result?.applied) {
    parts.push(
      t("settings.transfer.feedback.configImported", {
        defaultValue: "配置数据已恢复",
      }),
    );
  }
  if (result.config_result?.needs_restart) {
    parts.push(
      t("settings.transfer.feedback.restartRequired", {
        defaultValue: "部分变更需重启应用后生效",
      }),
    );
  }
  return parts.length > 0
    ? parts.join(" · ")
    : t("settings.webdav.feedback.restoreEmpty", {
        defaultValue: "远端备份未恢复任何数据",
      });
}

export function CursorWebdavHeaderActions() {
  const { t } = useTranslation();
  const [feedback, setFeedback] = useState<HeaderFeedback | null>(null);
  const [uploading, setUploading] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const busy = uploading || syncing;

  const ensureSavedConfig = useCallback(async () => {
    const settings = await getWebdavSyncSettings();
    if (
      !settings.enabled ||
      !settings.username.trim() ||
      !settings.has_password
    ) {
      throw new Error(
        t("settings.webdav.errors.notEnabled", {
          defaultValue: "请先启用 WebDAV 同步",
        }),
      );
    }
    return settings;
  }, [t]);

  const handleUpload = useCallback(async () => {
    setUploading(true);
    setFeedback({
      tone: "loading",
      text: t("settings.webdav.feedback.uploading", {
        defaultValue: "正在生成并上传备份...",
      }),
    });
    try {
      await ensureSavedConfig();
      const backupSettings = await getAutoBackupSettings();
      const backup = await createManagedBackup({
        trigger: "manual",
        selection: getSelectionFromAutoBackupSettings(backupSettings),
        retentionDays: backupSettings.retention_days,
        markAsLastRun: true,
      });
      const result = await uploadAutoBackupToWebdav(backup.file_name);
      setFeedback({
        tone: "success",
        text: t("settings.webdav.feedback.uploadSuccess", {
          count: result.uploaded_files.length,
          deleted: result.deleted_files.length,
          defaultValue:
            "已上传 {{count}} 个文件，清理 {{deleted}} 个远端过期备份",
        }),
      });
    } catch (error) {
      setFeedback({
        tone: "error",
        text: t("settings.webdav.feedback.uploadFailed", {
          error: normalizeError(error),
          defaultValue: "WebDAV 上传失败：{{error}}",
        }),
      });
    } finally {
      setUploading(false);
    }
  }, [ensureSavedConfig, t]);

  const handleSync = useCallback(async () => {
    setSyncing(true);
    setFeedback({
      tone: "loading",
      text: t("settings.webdav.feedback.syncingLatest", {
        defaultValue: "正在同步远端最新配置...",
      }),
    });
    try {
      await ensureSavedConfig();
      const files = await listWebdavBackupFiles();
      const latestBackup = files
        .filter((file) => file.file_kind === "json" || file.file_kind === "zip")
        .sort((a, b) => {
          const timeA = a.modified_at ? new Date(a.modified_at).getTime() : 0;
          const timeB = b.modified_at ? new Date(b.modified_at).getTime() : 0;
          return timeB - timeA;
        })[0];

      if (!latestBackup) {
        setFeedback({
          tone: "error",
          text: t("settings.webdav.feedback.noRemoteBackup", {
            defaultValue: "远端未发现可同步的备份文件",
          }),
        });
        return;
      }

      if (
        !window.confirm(
          t("settings.webdav.syncLatestConfirm", {
            name: latestBackup.file_name,
            defaultValue:
              "确认拉取最新的远端备份 {{name}} 并同步到本地？此操作将覆盖本地现有数据。",
          }),
        )
      ) {
        setFeedback(null);
        return;
      }

      setFeedback({
        tone: "loading",
        text: t("settings.webdav.feedback.restoring", {
          defaultValue: "正在读取远端备份并导入...",
        }),
      });

      const content = await readWebdavBackupFile(latestBackup.file_name);
      const result = await importDataTransferJson(content, {
        includeAccounts: true,
        includeConfig: true,
      });
      try {
        const store = useCursorAccountStore.getState();
        await store.fetchAccounts();
        await store.fetchCurrentAccountId();
      } catch {
        // 导入已经完成，列表刷新失败时仍保留成功提示。
      }
      setFeedback({
        tone: "success",
        text: buildImportFeedback(result, t),
      });
    } catch (error) {
      setFeedback({
        tone: "error",
        text: t("settings.webdav.feedback.syncFailed", {
          error: normalizeError(error),
          defaultValue: "同步最新配置失败：{{error}}",
        }),
      });
    } finally {
      setSyncing(false);
    }
  }, [ensureSavedConfig, t]);

  return (
    <div className="cursor-webdav-header-actions">
      <div className="cursor-webdav-header-actions-row">
        <button
          type="button"
          className="btn btn-primary"
          disabled={busy}
          title={t("settings.webdav.uploadAction", "生成并上传")}
          onClick={() => void handleUpload()}
        >
          {uploading ? (
            <RefreshCw size={14} className="loading-spinner" />
          ) : (
            <Upload size={14} />
          )}
          {t("settings.webdav.headerUploadAction", "上传")}
        </button>
        <button
          type="button"
          className="btn btn-secondary"
          disabled={busy}
          title={t("settings.webdav.syncAction", "同步配置")}
          onClick={() => void handleSync()}
        >
          <RefreshCw
            size={14}
            className={syncing ? "loading-spinner" : undefined}
          />
          {t("settings.webdav.headerSyncAction", "同步")}
        </button>
      </div>
      {feedback ? (
        <div
          className={`cursor-webdav-header-feedback add-feedback ${feedback.tone}`}
          role="status"
        >
          {feedback.text}
        </div>
      ) : null}
    </div>
  );
}
