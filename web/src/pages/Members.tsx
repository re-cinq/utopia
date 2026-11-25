import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Pencil, Plus, Search } from "lucide-react";
import { api } from "../api";
import { S } from "../i18n";
import { memberChanges } from "./memberChanges";
import {
  Button,
  Checkbox,
  Chip,
  DangerConfirm,
  Dialog,
  Dropdown,
  Field,
  FormDialog,
  IconButton,
  Input,
  Pager,
  REVEAL,
  SearchSelect,
  TBody,
  THead,
  Table,
  Td,
  Th,
  Tr,
  cn,
  pageSlice,
} from "../ui";
import { fetchMe } from "../authSession";

const ROLES = ["owner", "admin", "editor", "viewer"] as const;
const ROLE_OPTIONS = ROLES.map((r) => ({ value: r, label: S.members.roles[r] }));
const MEMBER_PAGE = 10;

/** A row in the roster. A deactivated account is also a row: it has no workspace role, otherwise identical */
type Person = {
  user_id: string;
  display_name: string;
  email: string;
  role: string;
  is_admin: boolean;
  deactivated: boolean;
};

/** The dialog for one member: changing identity is in the body, deactivate and remove are at the bottom left.
 *
 *  **Deactivate and remove are two different things**, so they are two buttons rather than one: the former cuts this person's access across the whole
 *  deployment (only an administrator can do it, the blast radius is much larger, and it goes through one more confirmation),
 *  while the latter only means he is no longer in this workspace. Squeezing them into one button would make people guess what a click will do. */
function MemberDialog({
  member,
  canDeactivate,
  canSetAdmin,
  isSelf,
  busy,
  onSave,
  onDeactivate,
  onRemove,
  onClose,
}: {
  member: Person;
  canDeactivate: boolean;
  canSetAdmin: boolean;
  isSelf: boolean;
  busy: boolean;
  onSave: (changes: ReturnType<typeof memberChanges>) => void;
  onDeactivate: () => void;
  onRemove: () => void;
  onClose: () => void;
}) {
  const [role, setRole] = useState(member.role);
  const [isAdmin, setIsAdmin] = useState(member.is_admin);
  const changes = memberChanges(member, { role, is_admin: isAdmin });
  return (
    <FormDialog
      title={member.display_name}
      description={member.email}
      width="sm"
      closeLabel={S.members.close}
      saveLabel={S.members.save}
      cancelLabel={S.members.cancel}
      canSave={Object.keys(changes).length > 0}
      busy={busy}
      onSave={() => onSave(changes)}
      onCancel={onClose}
      danger={[
        ...(canDeactivate
          ? [
              {
                label: S.members.deactivate,
                title: S.members.deactivateHint,
                onClick: onDeactivate,
              },
            ]
          : []),
        { label: S.members.remove, onClick: onRemove },
      ]}
    >
      <Field label={S.members.roleLabel}>
        <Dropdown
          className="w-full"
          value={role}
          onChange={setRole}
          options={ROLE_OPTIONS}
        />
      </Field>
      {canSetAdmin && (
        <Checkbox
          label={S.members.systemAdmin}
          hint={S.members.systemAdminHint}
          checked={isAdmin}
          disabled={isSelf && member.is_admin}
          onChange={setIsAdmin}
        />
      )}
    </FormDialog>
  );
}

export function Members({ workspaceId }: { workspaceId: string }) {
  const queryClient = useQueryClient();
  const [addUserId, setAddUserId] = useState("");
  const [addRole, setAddRole] = useState("viewer");
  const [memberPage, setMemberPage] = useState(0);
  const [filter, setFilter] = useState("");
  // Active only, deactivated only, or all. The default is active only: that is this page's usual question
  const [status, setStatus] = useState<"all" | "active" | "deactivated">("active");
  const [creating, setCreating] = useState(false);
  const [adding, setAdding] = useState(false);
  // A role is normally a line of text, and only becomes a dropdown when clicked: only one row is being edited at a time
  // Filter by role
  const [role, setRole] = useState("all");
  const me = useQuery({ queryKey: ["me"], queryFn: fetchMe });
  const [error, setError] = useState<string | null>(null);

  const members = useQuery({
    queryKey: ["members", workspaceId],
    queryFn: () => api.members(workspaceId),
  });
  const orgUsers = useQuery({ queryKey: ["orgUsers"], queryFn: api.orgUsers });
  /* Deactivated accounts are at deployment level and cannot be found in the member table (after deactivation the person disappears from the member table, the person picker,
     and every list), so they are fetched separately and merged into the same roster. **Only administrators can see them**:
     only they can restore them */
  const deactivated = useQuery({
    queryKey: ["deactivatedUsers"],
    queryFn: api.deactivatedUsers,
    enabled: !!me.data?.is_admin,
  });

  const refresh = () => {
    setError(null);
    queryClient.invalidateQueries({ queryKey: ["members", workspaceId] });
  };
  const onError = (e: unknown) => setError((e as Error).message);

  const setRole_ = useMutation({
    mutationFn: ({ userId, role }: { userId: string; role: string }) =>
      api.setMemberRole(workspaceId, userId, role),
    onSuccess: refresh,
    onError,
  });
  const setSystemAdmin = useMutation({
    mutationFn: ({ userId, isAdmin }: { userId: string; isAdmin: boolean }) =>
      api.adminSetSystemAdmin(userId, isAdmin),
    onSuccess: refresh,
    onError,
  });
  const remove = useMutation({
    mutationFn: (userId: string) => api.removeMember(workspaceId, userId),
    onSuccess: refresh,
    onError,
  });
  // Deactivating asks first: use the site-wide dialog rather than the browser's native confirm()
  // The member being edited (the pencil at the end of the row opens it)
  const [editing, setEditing] = useState<Person | null>(null);
  const [deactivating, setDeactivating] = useState<{ id: string; name: string } | null>(
    null,
  );
  const deactivate = useMutation({
    mutationFn: (userId: string) => api.adminDeactivateUser(userId),
    onSuccess: () => {
      refresh();
      queryClient.invalidateQueries({ queryKey: ["deactivatedUsers"] });
    },
    onError,
  });
  const revive = useMutation({
    mutationFn: (userId: string) => api.adminReactivateUser(userId),
    onSuccess: () => {
      refresh();
      queryClient.invalidateQueries({ queryKey: ["deactivatedUsers"] });
      queryClient.invalidateQueries({ queryKey: ["orgUsers"] });
    },
    onError,
  });

  const memberIds = new Set(members.data?.map((m) => m.user_id));
  const addable = orgUsers.data?.filter((u) => !memberIds.has(u.id)) ?? [];

  /* **Deactivated is not a separate table; it is one state within this roster.**
     Previously it hung on its own block at the page foot: the same person appeared in two places, while "does this account still exist"
     is exactly the first question when looking at the roster. Merged into one list, it becomes a status you can filter by */
  const people: Person[] = [
    ...(members.data ?? []).map((m) => ({ ...m, deactivated: false })),
    ...(deactivated.data ?? []).map((u) => ({
      user_id: u.id,
      display_name: u.display_name,
      email: u.email,
      role: "",
      is_admin: u.is_admin,
      deactivated: true,
    })),
  ];
  const q = filter.trim().toLowerCase();
  const memberList = people
    .filter((p) => status === "all" || (status === "deactivated") === p.deactivated)
    .filter((p) => role === "all" || p.role === role)
    .filter(
      (p) =>
        !q ||
        p.display_name.toLowerCase().includes(q) ||
        p.email.toLowerCase().includes(q),
    )
    // Deactivated ones sink to the bottom: they are still listed, but should not rank ahead of people still in use
    .sort((a, b) => Number(a.deactivated) - Number(b.deactivated));
  const { rows: pagedMembers, safe: safeMemberPage } = pageSlice(memberList, memberPage, MEMBER_PAGE);

  const reset = (fn: () => void) => {
    fn();
    setMemberPage(0);
  };

  return (
    <div className="space-y-4">
      {/* The things that filter this roster sit outside the table (DESIGN.md 6): three filters of the same shape:
          a medium input box with a magnifier + two dropdowns, consistent with the filter bars on the graph and library pages */}
      <div className="flex flex-wrap items-center gap-2">
        <Input
          icon={<Search size={13} />}
          className="w-64"
          placeholder={S.settings.searchUsers}
          value={filter}
          onChange={(e) => reset(() => setFilter(e.target.value))}
        />
        <Dropdown
          className="w-32"
          value={role}
          onChange={(v) => reset(() => setRole(v))}
          options={[
            { value: "all", label: S.members.filterAllRoles },
            ...ROLE_OPTIONS,
          ]}
        />
        {me.data?.is_admin && (
          <Dropdown
            className="w-36"
            value={status}
            onChange={(v) => reset(() => setStatus(v as typeof status))}
            options={[
              { value: "active", label: S.members.filterActive },
              { value: "deactivated", label: S.members.filterDeactivated },
              { value: "all", label: S.members.filterAll },
            ]}
          />
        )}
        <div className="ml-auto flex items-center gap-2">
          <Button variant="secondary" size="sm" onClick={() => setAdding(true)}>
            {S.members.addExisting}
          </Button>
          {me.data?.is_admin && (
            <Button variant="primary" size="sm" onClick={() => setCreating(true)}>
              <Plus size={12} />
              {S.settings.newUser}
            </Button>
          )}
        </div>
      </div>

      {error && <p className="text-body text-danger">{error}</p>}

      {/* One table: each column's width is fixed, and the role and actions each sit in their own column, so they no longer drift with name length */}
      <div className="glass rounded-panel overflow-hidden">
        <Table>
          <THead>
            <Tr>
              <Th>{S.members.userLabel}</Th>
              <Th>{S.members.roleLabel}</Th>
              <Th>{S.members.statusLabel}</Th>
              <Th />
            </Tr>
          </THead>
          <TBody>
            {pagedMembers.map((m) => (
              <Tr
                key={m.user_id}
                // group: the ⋯ at the end of the row uses it to recognize "the pointer is on this row" (see REVEAL)
                className={cn("group", m.deactivated && "opacity-55")}
              >
                <Td>
                  <div className="flex items-center gap-2">
                    <span className="truncate text-body text-ink">{m.display_name}</span>
                    {m.is_admin && <Chip tone="info">{S.members.systemAdmin}</Chip>}
                  </div>
                  <div className="truncate text-small text-ink-2">{m.email}</div>
                </Td>
                <Td className="text-ink-2">
                  {/* **It is just one word.** Changing the role has moved into the menu at the end of the row: this column is the roster
                      stating facts, not a row of controls waiting to be filled in */}
                  {m.deactivated
                    ? "—"
                    : (S.members.roles[m.role as keyof typeof S.members.roles] ??
                      m.role)}
                </Td>
                <Td>
                  {m.deactivated ? (
                    <Chip tone="danger">{S.members.filterDeactivated}</Chip>
                  ) : (
                    <span className="text-small text-ink-2">{S.members.filterActive}</span>
                  )}
                </Td>
                {/* **A row's actions are folded into the pencil at the end of the row**: they only appear when the pointer is on the row
                    (`REVEAL`), and clicking it opens a centred dialog, where changing the role, deactivating and removing are all done.
                    Previously two or three permanent red labels were spread at the end of each row, so ten rows meant twenty of them,
                    and the most eye-catching thing on one screen became "remove" and "deactivate", while nine times out of ten people visiting this page
                    were only checking who is in it. */}
                <Td className="text-right">
                  {/* A deactivated account has no workspace role, so there is nothing to edit: its row
                      has only one thing to do, so show it directly, without hiding it in a dialog */}
                  {m.deactivated ? (
                    <Button variant="secondary" size="sm"
                      disabled={revive.isPending}
                      onClick={() => revive.mutate(m.user_id)}
                    >
                      {S.members.reactivate}
                    </Button>
                  ) : (
                    <IconButton
                      size="sm"
                      label={S.members.editMember}
                      className={REVEAL}
                      onClick={() => setEditing(m)}
                    >
                      <Pencil size={13} />
                    </IconButton>
                  )}
                </Td>
              </Tr>
            ))}
          </TBody>
        </Table>
        {memberList.length === 0 && (
          <p className="px-4 py-6 text-body text-ink-2">{S.ui.noMatches}</p>
        )}
      </div>
      <Pager
        total={memberList.length}
        pageSize={MEMBER_PAGE}
        page={safeMemberPage}
        onPage={setMemberPage}
      />

      {/* Add an **existing account** to this workspace. This is a different thing from "create an account": that one makes a person from nothing,
          this one just gives an already existing person a role */}
      <Dialog
        open={adding}
        onOpenChange={setAdding}
        title={S.members.addExisting}
        closeLabel={S.ui.close}
        footer={
          <>
            <Button variant="secondary" size="sm" onClick={() => setAdding(false)}>
              {S.members.cancel}
            </Button>
            <Button variant="primary" size="sm"
              disabled={!addUserId || setRole_.isPending}
              onClick={() => {
                setRole_.mutate({ userId: addUserId, role: addRole });
                setAdding(false);
              }}
            >
              {S.members.add}
            </Button>
          </>
        }
      >
        <div className="grid grid-cols-2 gap-3">
          <Field label={S.members.userLabel} className="mb-0">
            {/* An empty list is explained by the SearchSelect itself (it has a noMatches empty state) */}
            <SearchSelect
              className="w-full"
              value={addUserId}
              onChange={setAddUserId}
              placeholder={S.members.pickUser}
              options={addable.map((u) => ({
                value: u.id,
                label: u.display_name,
                hint: u.email,
              }))}
            />
          </Field>
          <Field label={S.members.roleLabel} className="mb-0">
            <Dropdown
              className="w-full"
              value={addRole}
              onChange={setAddRole}
              options={ROLE_OPTIONS}
            />
          </Field>
        </div>
      </Dialog>

      {me.data?.is_admin && (
        <CreateUserDialog
          open={creating}
          onOpenChange={setCreating}
          onCreated={() => {
            setCreating(false);
            refresh();
          }}
        />
      )}
      {editing && (
        <MemberDialog
          member={editing}
          canDeactivate={!!me.data?.is_admin && me.data.id !== editing.user_id}
          canSetAdmin={!!me.data?.is_admin}
          isSelf={me.data?.id === editing.user_id}
          busy={setRole_.isPending || setSystemAdmin.isPending || remove.isPending}
          onSave={({ role, is_admin }) => {
            if (role !== undefined) setRole_.mutate({ userId: editing.user_id, role });
            if (is_admin !== undefined)
              setSystemAdmin.mutate({ userId: editing.user_id, isAdmin: is_admin });
            setEditing(null);
          }}
          onDeactivate={() => {
            setDeactivating({ id: editing.user_id, name: editing.display_name });
            setEditing(null);
          }}
          onRemove={() => {
            remove.mutate(editing.user_id);
            setEditing(null);
          }}
          onClose={() => setEditing(null)}
        />
      )}

      {deactivating && (
        <DangerConfirm
          title={S.members.deactivate}
          hint={S.members.deactivateConfirm(deactivating.name)}
          confirmLabel={S.members.deactivate}
          cancelLabel={S.members.cancel}
          busy={deactivate.isPending}
          onConfirm={() => {
            deactivate.mutate(deactivating.id);
            setDeactivating(null);
          }}
          onCancel={() => setDeactivating(null)}
        />
      )}
    </div>
  );
}

/** An administrator creates an account on someone's behalf (the only entry point once registration is closed). Creating an account is an action, so it lives in a dialog */
function CreateUserDialog({
  open,
  onOpenChange,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: () => void;
}) {
  const queryClient = useQueryClient();
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [role, setRole] = useState("editor");
  const [error, setError] = useState<string | null>(null);

  const create = useMutation({
    mutationFn: () =>
      api.adminCreateUser({ email: email.trim(), display_name: name.trim(), password, role }),
    onSuccess: () => {
      setEmail("");
      setName("");
      setPassword("");
      setError(null);
      queryClient.invalidateQueries({ queryKey: ["orgUsers"] });
      onCreated();
    },
    onError: (e) => setError((e as Error).message),
  });

  const valid = email.includes("@") && name.trim() && password.length >= 8;

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title={S.settings.newUser}
      closeLabel={S.ui.close}
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={() => onOpenChange(false)}>
            {S.members.cancel}
          </Button>
          <Button variant="primary" size="sm"
            disabled={!valid || create.isPending}
            onClick={() => create.mutate()}
          >
            {S.settings.createUserBtn}
          </Button>
        </>
      }
    >
      {/* This is **creating an account for someone else**, not logging in: the browser, seeing "email + password", fills in the currently
          logged-in person (when the administrator opens it, what they see is their own name and a row of dots).
          `new-password` lets Chrome recognize that this is setting a new password, not back-filling old credentials;
          the two fields above also turn off autofill */}
      <div className="grid grid-cols-2 gap-3">
        <Field label={S.login.email} className="mb-0">
          <Input className="w-full"
            autoFocus
            autoComplete="off"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
          />
        </Field>
        <Field label={S.login.displayName} className="mb-0">
          <Input className="w-full"
            autoComplete="off"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </Field>
        <Field label={S.settings.initialPassword} className="mb-0">
          <Input className="w-full"
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </Field>
        <Field label={S.members.roleLabel} className="mb-0">
          <Dropdown
            className="w-full"
            value={role}
            onChange={setRole}
            options={[
              { value: "admin", label: S.members.roles.admin },
              { value: "editor", label: S.members.roles.editor },
              { value: "viewer", label: S.members.roles.viewer },
            ]}
          />
        </Field>
      </div>
      {error && <p className="mt-3 text-small text-danger">{error}</p>}
    </Dialog>
  );
}
