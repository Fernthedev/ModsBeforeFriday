

type Request = GetModStatus | Patch;

interface GetModStatus {
    type: 'GetModStatus'
}

interface Patch {
    type: 'Patch'
}

type Response = ModStatus;

interface ModStatus {
    type: 'ModStatus',
    app_info: AppInfo | null
}

interface AppInfo {
    version: string,
    is_modded: boolean
}

export type {
    Request,
    GetModStatus,
    Response,
    ModStatus,
    AppInfo
}