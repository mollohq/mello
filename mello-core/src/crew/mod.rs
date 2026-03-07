//! Crew (group) management

pub type CrewId = String;
pub type MemberId = String;

#[derive(Debug, Clone)]
pub struct Crew {
    pub id: CrewId,
    pub name: String,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub id: MemberId,
    pub name: String,
    pub online: bool,
}
