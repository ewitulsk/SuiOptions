/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_DAKOTA_API?: string;
  readonly VITE_AUTH_API?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
