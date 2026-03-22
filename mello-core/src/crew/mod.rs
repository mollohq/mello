pub type CrewId = String;
pub type MemberId = String;

#[derive(Debug, Clone)]
pub struct Crew {
    pub id: CrewId,
    pub name: String,
    pub description: String,
    pub member_count: i32,
    pub max_members: i32,
    pub open: bool,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub id: MemberId,
    pub username: String,
    pub display_name: String,
    pub online: bool,
}
