type MemberAccess = { role: string; is_admin: boolean };

/** The changes that are actually sent when the dialog saves: fields that did not change are not sent */
export function memberChanges(member: MemberAccess, draft: MemberAccess): Partial<MemberAccess> {
  return {
    ...(draft.role !== member.role && { role: draft.role }),
    ...(draft.is_admin !== member.is_admin && { is_admin: draft.is_admin }),
  };
}
