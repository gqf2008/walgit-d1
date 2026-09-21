import { StrictMode, lazy } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import { Layout } from "./components/Layout";
import { Home } from "./pages/Home";
import { Repos } from "./pages/Repos";
import { ReposIndex } from "./pages/ReposIndex";
import { RepoLayout } from "./pages/RepoLayout";
import { TreePage } from "./pages/TreePage";
import { CommitsPage } from "./pages/CommitsPage";
import { track } from "./data";
import { I18nProvider } from "./i18n";
import "./styles.css";

// Heavy pages (syntax highlighting / diff rendering / WAL dashboard) are split
// into their own chunks and only downloaded when a user navigates to them.
// `track` shows the chunk download in the top progress bar; the route
// boundaries in Layout/RepoLayout provide the Suspense fallbacks.
const BlobPage = lazy(() => track(import("./pages/BlobPage")).then((m) => ({ default: m.BlobPage })));
const CommitPage = lazy(() => track(import("./pages/CommitPage")).then((m) => ({ default: m.CommitPage })));
const OverviewPage = lazy(() => track(import("./pages/OverviewPage")).then((m) => ({ default: m.OverviewPage })));
const SettingsPage = lazy(() => track(import("./pages/SettingsPage")).then((m) => ({ default: m.SettingsPage })));
const ApiPage = lazy(() => track(import("./pages/ApiPage")).then((m) => ({ default: m.ApiPage })));
const CollabPage = lazy(() => track(import("./pages/CollabPage")).then((m) => ({ default: m.CollabPage })));
const CollabBoardPage = lazy(() => track(import("./pages/CollabBoardPage")).then((m) => ({ default: m.CollabBoardPage })));
const CollabThreadPage = lazy(() => track(import("./pages/CollabThreadPage")).then((m) => ({ default: m.CollabThreadPage })));
const CollabGuidePage = lazy(() => track(import("./pages/CollabGuidePage")).then((m) => ({ default: m.CollabGuidePage })));
const SetupPage = lazy(() => track(import("./pages/SetupPage")).then((m) => ({ default: m.SetupPage })));

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <I18nProvider>
      <BrowserRouter>
      <Routes>
        <Route path="setup" element={<SetupPage />} />
        <Route element={<Layout />}>
          <Route index element={<Home />} />
          <Route path="repos" element={<ReposIndex />} />
          <Route path="api" element={<ApiPage />} />
          <Route path=":owner" element={<Repos />} />
          <Route path=":owner/:repo" element={<RepoLayout />}>
            <Route index element={<TreePage />} />
            <Route path="tree/*" element={<TreePage />} />
            <Route path="blob/*" element={<BlobPage />} />
            <Route path="wal" element={<OverviewPage />} />
            <Route path="settings" element={<SettingsPage />} />
            <Route path="commits" element={<CommitsPage />} />
            <Route path="commits/*" element={<CommitsPage />} />
            <Route path="commit/:sha" element={<CommitPage />} />
            <Route path="collab" element={<CollabPage />} />
            <Route path="collab/board" element={<CollabBoardPage />} />
            <Route path="collab/guide" element={<CollabGuidePage />} />
            <Route path="collab/thread/:id" element={<CollabThreadPage />} />
          </Route>
        </Route>
      </Routes>
      </BrowserRouter>
    </I18nProvider>
  </StrictMode>,
);
